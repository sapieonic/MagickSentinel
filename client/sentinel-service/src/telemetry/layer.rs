//! The `tracing` layer and the background exporter thread.
//!
//! One bounded channel, one thread. The layer's `on_event` does the minimum — build a
//! [`Record`], try to enqueue it — because it runs on whatever thread emitted the
//! event, and on the agent that is the single 20 ms capture loop. An exporter that
//! blocked there would insert network latency between reading an audio buffer and
//! writing it to the spool.

use super::otlp::{self, AttrValue, Record, Resource, Severity};
use super::{attribute_permitted, Shipper, TARGET};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// Records held in memory before the exporter thread picks them up.
///
/// Bounded, because the endpoint generating the most telemetry is the one that has just
/// lost its uplink, and that is not the moment to start taking memory away from
/// capture. 1024 records is a few hundred kilobytes at the sizes these carry, and about
/// ten minutes of a busy endpoint's state changes.
pub const QUEUE_CAPACITY: usize = 1024;

/// Records per HTTP request.
pub const MAX_BATCH: usize = 128;

/// How long a partial batch waits before it is sent anyway.
///
/// Thirty seconds, matching the heartbeat interval: the two carry overlapping
/// information and there is no value in this one being fresher than the signal an
/// operator actually watches.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// How long the exporter waits for a final flush at shutdown.
///
/// Short. Telemetry is diagnostic; holding a service stop open for it would turn a
/// failing collector into a service that Windows reports as hung.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Handle to the exporter thread. Dropping it stops the thread.
pub struct TelemetryHandle {
    /// Dropped to close the channel, which is how the thread learns to stop.
    sender: Option<SyncSender<Record>>,
    thread: Option<std::thread::JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

impl TelemetryHandle {
    /// Records dropped because the queue was full. Reported in the next batch as well;
    /// exposed here so a test and the heartbeat can both see it.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stop the exporter, giving it [`SHUTDOWN_GRACE`] to flush.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        // Closing the channel is the stop signal: the thread's `recv` returns
        // `Disconnected`, it flushes what it has, and it exits.
        self.sender = None;
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            // Deliberately not joined past the deadline. A collector that has stopped
            // answering must not be able to hold a service stop open — Windows would
            // report the service as hung and the SCM would kill it, which looks like a
            // crash in the fleet view.
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

impl Drop for TelemetryHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The `tracing` layer.
pub struct OtlpLayer {
    sender: SyncSender<Record>,
    dropped: Arc<AtomicU64>,
}

impl OtlpLayer {
    /// Build the layer and start the exporter thread.
    ///
    /// Returns the layer to install in the subscriber and a handle whose drop stops the
    /// thread. The caller keeps the handle alive for as long as it wants telemetry;
    /// `main` holds it for the life of the process.
    pub fn new(resource: Resource, shipper: Box<dyn Shipper>) -> (Self, TelemetryHandle) {
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let thread_dropped = dropped.clone();
        let thread = std::thread::Builder::new()
            .name("sentinel-otlp".into())
            .spawn(move || export_loop(receiver, resource, shipper, thread_dropped))
            .ok();
        (
            OtlpLayer { sender: sender.clone(), dropped: dropped.clone() },
            TelemetryHandle { sender: Some(sender), thread, dropped },
        )
    }
}

/// Drain the queue in batches until the channel closes.
fn export_loop(
    receiver: Receiver<Record>,
    resource: Resource,
    shipper: Box<dyn Shipper>,
    dropped: Arc<AtomicU64>,
) {
    let mut batch: Vec<Record> = Vec::with_capacity(MAX_BATCH);
    let mut reported_drops: u64 = 0;
    let mut endpoint_failing = false;
    loop {
        match receiver.recv_timeout(FLUSH_INTERVAL) {
            Ok(record) => {
                batch.push(record);
                if batch.len() < MAX_BATCH {
                    continue;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                flush(shipper.as_ref(), &resource, &mut batch, &dropped, &mut reported_drops, &mut endpoint_failing);
                return;
            }
        }
        flush(shipper.as_ref(), &resource, &mut batch, &dropped, &mut reported_drops, &mut endpoint_failing);
    }
}

fn flush(
    shipper: &dyn Shipper,
    resource: &Resource,
    batch: &mut Vec<Record>,
    dropped: &AtomicU64,
    reported_drops: &mut u64,
    endpoint_failing: &mut bool,
) {
    // Report the drop count the same way the spool reports eviction: as data, not as a
    // silence. A telemetry stream with holes in it that does not say so is worse than
    // no telemetry, because it looks complete.
    let total_dropped = dropped.load(Ordering::Relaxed);
    if total_dropped > *reported_drops {
        batch.push(Record {
            time_unix_nano: now_unix_nano(),
            severity: Severity::Warn,
            body: "telemetry records were dropped because the export queue was full".into(),
            attributes: vec![
                ("event".into(), AttrValue::Str("telemetry.dropped".into())),
                (
                    "dropped".into(),
                    AttrValue::Int((total_dropped - *reported_drops) as i64),
                ),
            ],
        });
        *reported_drops = total_dropped;
    }
    if batch.is_empty() {
        return;
    }
    let payload = otlp::encode(resource, batch);
    batch.clear();
    match shipper.ship(&payload) {
        Ok(()) => {
            if *endpoint_failing {
                tracing::info!(endpoint = shipper.endpoint(), "telemetry export recovered");
                *endpoint_failing = false;
            }
        }
        Err(e) => {
            // NOT on the telemetry target: an export failure logged to the exported
            // target would enqueue a record whose failure enqueues another. Plain
            // `tracing` only, and once per transition rather than once per batch.
            if !*endpoint_failing {
                tracing::warn!(
                    endpoint = shipper.endpoint(),
                    error = %e,
                    "telemetry export is failing; batches will be dropped until it recovers"
                );
                *endpoint_failing = true;
            }
        }
    }
}

pub(crate) fn now_unix_nano() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        // A clock behind the epoch is a machine whose CMOS battery died. Zero is a
        // visibly wrong timestamp, which is better than a panic in a log path.
        .unwrap_or(0)
}

impl<S: Subscriber> Layer<S> for OtlpLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // The containment boundary: one target, checked first, before any allocation.
        if event.metadata().target() != TARGET {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let record = Record {
            time_unix_nano: now_unix_nano(),
            severity: Severity::from_tracing(event.metadata().level()),
            body: visitor.message.unwrap_or_else(|| event.metadata().name().to_string()),
            attributes: visitor.attributes,
        };
        if let Err(TrySendError::Full(_)) = self.sender.try_send(record) {
            // Never block. This runs on the emitting thread, which on the agent is the
            // single 20 ms capture loop.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Turns `tracing` fields into OTLP attributes, dropping the forbidden ones.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    attributes: Vec<(String, AttrValue)>,
}

impl FieldVisitor {
    fn push(&mut self, field: &Field, value: AttrValue) {
        if field.name() == "message" {
            if let AttrValue::Str(s) = value {
                self.message = Some(s);
            }
            return;
        }
        if !attribute_permitted(field.name()) {
            // Dropped, and said so — but without the value, which is the thing that
            // must not leave. The key alone is enough to find the offending call site.
            self.attributes.push((
                "telemetry.dropped_attribute".into(),
                AttrValue::Str(field.name().to_string()),
            ));
            return;
        }
        self.attributes.push((field.name().to_string(), value));
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, AttrValue::Str(value.to_string()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, AttrValue::Int(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        // Saturating rather than wrapping: OTLP's intValue is signed, and a spool depth
        // that appeared as a negative number would be read as a bug in the dashboard.
        self.push(field, AttrValue::Int(value.min(i64::MAX as u64) as i64));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, AttrValue::Bool(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field, AttrValue::Float(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, AttrValue::Str(format!("{value:?}")));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.push(field, AttrValue::Str(value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tracing_subscriber::prelude::*;

    #[derive(Default)]
    struct Collecting {
        payloads: Mutex<Vec<Vec<u8>>>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl Shipper for Collecting {
        fn ship(&self, payload: &[u8]) -> Result<(), String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("404 Not Found".into());
            }
            self.payloads.lock().unwrap().push(payload.to_vec());
            Ok(())
        }
        fn endpoint(&self) -> &str {
            "https://api.example.com/v1/telemetry/otlp/v1/logs"
        }
    }

    /// The shipper is moved into the exporter thread, so tests observe it through an
    /// `Arc` they keep a second reference to.
    struct SharedShipper(Arc<Collecting>);

    impl Shipper for SharedShipper {
        fn ship(&self, payload: &[u8]) -> Result<(), String> {
            self.0.ship(payload)
        }
        fn endpoint(&self) -> &str {
            self.0.endpoint()
        }
    }

    fn resource() -> Resource {
        Resource {
            service_name: "sentinel-agent".into(),
            service_version: "0.1.0".into(),
            tenant_id: Some("t-acme".into()),
            device_id: Some("dev-1".into()),
        }
    }

    /// Emit `f` under a subscriber carrying the layer, then stop the exporter so its
    /// final flush has happened before the assertions run.
    fn capture(f: impl FnOnce()) -> Vec<serde_json::Value> {
        let collecting = Arc::new(Collecting::default());
        let (layer, handle) = OtlpLayer::new(resource(), Box::new(SharedShipper(collecting.clone())));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, f);
        handle.shutdown();
        let payloads = collecting.payloads.lock().unwrap();
        payloads
            .iter()
            .map(|p| serde_json::from_slice(p).expect("valid OTLP JSON"))
            .collect()
    }

    fn records(batches: &[serde_json::Value]) -> Vec<serde_json::Value> {
        batches
            .iter()
            .flat_map(|b| {
                b["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn only_events_on_the_telemetry_target_are_exported() {
        // The containment boundary. Adding a log line anywhere in the client must not
        // start shipping it off the machine.
        let batches = capture(|| {
            tracing::info!("an ordinary log line");
            tracing::warn!(spool.depth = 7u64, "another ordinary one");
            tracing::info!(target: super::TARGET, event = "capture.armed", "capture armed");
        });
        let rs = records(&batches);
        assert_eq!(rs.len(), 1, "exactly the one targeted event: {rs:?}");
        assert_eq!(rs[0]["body"]["stringValue"], "capture armed");
    }

    #[test]
    fn fields_become_attributes_and_the_message_becomes_the_body() {
        let batches = capture(|| {
            tracing::warn!(
                target: super::TARGET,
                event = "capture.blocked",
                reason = "pinned_device_missing",
                spool.depth = 12u64,
                signed_in = true,
                "capture could not arm"
            );
        });
        let rs = records(&batches);
        assert_eq!(rs[0]["body"]["stringValue"], "capture could not arm");
        assert_eq!(rs[0]["severityText"], "WARN");
        let attrs: Vec<(String, serde_json::Value)> = rs[0]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["key"].as_str().unwrap().to_string(), a["value"].clone()))
            .collect();
        let get = |k: &str| attrs.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("event").unwrap()["stringValue"], "capture.blocked");
        assert_eq!(get("reason").unwrap()["stringValue"], "pinned_device_missing");
        assert_eq!(get("spool.depth").unwrap()["intValue"], "12");
        assert_eq!(get("signed_in").unwrap()["boolValue"], true);
    }

    #[test]
    fn a_forbidden_field_is_dropped_and_only_its_name_is_reported() {
        // A call site that attaches a UID must not be able to export it, and the
        // failure has to be findable — hence the key without the value.
        let batches = capture(|| {
            tracing::info!(
                target: super::TARGET,
                event = "auth.sign_in_state",
                user_uid = "KnA1secretuid",
                account_ref = "LN-88213",
                signed_in = true,
                "sign-in state changed"
            );
        });
        let rs = records(&batches);
        let json = serde_json::to_string(&rs).unwrap();
        assert!(!json.contains("KnA1secretuid"), "the UID leaked: {json}");
        assert!(!json.contains("LN-88213"), "the account reference leaked: {json}");
        assert!(json.contains("telemetry.dropped_attribute"));
        assert!(json.contains("user_uid"), "the field name is kept so the call site is findable");
        assert!(json.contains("\"boolValue\":true"), "the safe fields still went");
    }

    #[test]
    fn the_resource_names_the_service_on_every_batch() {
        let batches = capture(|| {
            tracing::info!(target: super::TARGET, event = "capture.armed", "armed");
        });
        let attrs = batches[0]["resourceLogs"][0]["resource"]["attributes"].clone();
        let json = serde_json::to_string(&attrs).unwrap();
        assert!(json.contains("sentinel-agent"));
        assert!(json.contains("t-acme"));
        assert!(json.contains("dev-1"));
    }

    #[test]
    fn a_full_queue_drops_records_and_says_how_many() {
        // The spool's rule applied to telemetry: a stream with holes that does not say
        // so is worse than no stream, because it looks complete.
        let collecting = Arc::new(Collecting::default());
        // Fail every ship so the exporter thread cannot drain, then overrun the queue.
        collecting.fail.store(true, Ordering::SeqCst);
        let (layer, handle) =
            OtlpLayer::new(resource(), Box::new(SharedShipper(collecting.clone())));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..(QUEUE_CAPACITY + MAX_BATCH + 500) {
                tracing::info!(target: super::TARGET, event = "spool.depth", i = i as u64, "depth");
            }
        });
        assert!(handle.dropped() > 0, "the bounded queue must drop rather than grow");

        // Once the endpoint recovers, the drop count is reported.
        collecting.fail.store(false, Ordering::SeqCst);
        handle.shutdown();
        let payloads = collecting.payloads.lock().unwrap();
        let batches: Vec<serde_json::Value> =
            payloads.iter().map(|p| serde_json::from_slice(p).unwrap()).collect();
        let json = serde_json::to_string(&batches).unwrap();
        assert!(json.contains("telemetry.dropped"), "the loss is reported: {}", &json[..json.len().min(400)]);
    }

    #[test]
    fn an_export_failure_does_not_recurse_into_the_export_queue() {
        // The failure is logged on plain `tracing`, not on the telemetry target: a
        // failure that enqueued a record whose failure enqueued another would spin.
        let collecting = Arc::new(Collecting::default());
        collecting.fail.store(true, Ordering::SeqCst);
        let (layer, handle) =
            OtlpLayer::new(resource(), Box::new(SharedShipper(collecting.clone())));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: super::TARGET, event = "capture.armed", "armed");
        });
        handle.shutdown();
        assert!(collecting.payloads.lock().unwrap().is_empty(), "nothing was accepted");
    }

    #[test]
    fn a_debug_formatted_field_is_stringified_rather_than_dropped() {
        let batches = capture(|| {
            tracing::info!(
                target: super::TARGET,
                event = "capture.tier_detected",
                tier = ?Some("B"),
                "tier detected"
            );
        });
        let json = serde_json::to_string(&records(&batches)).unwrap();
        assert!(json.contains("Some(\\\"B\\\")"), "got {json}");
    }
}
