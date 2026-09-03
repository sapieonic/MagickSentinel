//! OTLP/HTTP JSON encoding for log records.
//!
//! OpenTelemetry defines two encodings for OTLP over HTTP: protobuf and JSON. This is
//! the JSON one, because it needs no code generation step, no `.proto` files in the
//! tree, and no protobuf runtime inside a binary that is shipped to 200 desktops and
//! signed with an EV certificate. Every collector that accepts OTLP/HTTP accepts it.
//!
//! # Why log records and not spans
//!
//! Everything worth exporting from an endpoint is a state change at a point in time:
//! capture armed, capture refused to arm and here is why, the spool evicted N
//! segments, the uplink reconnected, tier detection came out B, the user signed in.
//! None of those is a unit of work with a duration and a parent, which is what a span
//! is for. Modelling them as spans would mean emitting zero-length spans with no
//! parents — a trace view full of dots — and it would put trace and span id generation
//! and propagation into a client that has nothing to propagate them to. Log records
//! carry a timestamp, a severity, a body and attributes, which is exactly the shape of
//! the data. If a genuine cross-process trace ever exists here (the enrollment exchange
//! is the only candidate) spans can be added alongside; nothing in this module
//! precludes it.
//!
//! # Numbers as strings
//!
//! `timeUnixNano` is a string, not a number. That is proto3's JSON mapping for 64-bit
//! integers, and it exists because JSON numbers are IEEE 754 doubles: a nanosecond
//! timestamp is around 1.8e18 and loses its last two digits as a double. Collectors
//! reject or silently mangle a numeric one.

use serde_json::{json, Map, Value};

/// An attribute value. Deliberately small: these are machine facts, and every
//  additional shape is another thing a reviewer has to check for PII.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Bool(bool),
    Float(f64),
}

impl AttrValue {
    fn to_any_value(&self) -> Value {
        match self {
            AttrValue::Str(s) => json!({"stringValue": s}),
            // Same 64-bit-in-JSON problem as the timestamp: OTLP's `intValue` is a
            // string. A spool depth will never be large enough to matter, but a
            // collector that validates the type will reject a number.
            AttrValue::Int(i) => json!({"intValue": i.to_string()}),
            AttrValue::Bool(b) => json!({"boolValue": b}),
            AttrValue::Float(f) => json!({"doubleValue": f}),
        }
    }
}

/// Severity, as OpenTelemetry numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Trace = 1,
    Debug = 5,
    Info = 9,
    Warn = 13,
    Error = 17,
}

impl Severity {
    pub fn from_tracing(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Severity::Trace,
            tracing::Level::DEBUG => Severity::Debug,
            tracing::Level::INFO => Severity::Info,
            tracing::Level::WARN => Severity::Warn,
            tracing::Level::ERROR => Severity::Error,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Severity::Trace => "TRACE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
        }
    }
}

/// One exportable record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Nanoseconds since the Unix epoch.
    pub time_unix_nano: u128,
    pub severity: Severity,
    /// The event's message. Kept as the body rather than as an attribute so a
    /// collector's default view shows something readable.
    pub body: String,
    pub attributes: Vec<(String, AttrValue)>,
}

/// Identity of the emitting process, sent once per batch.
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    /// `sentinel-agent` or `sentinel-service`.
    pub service_name: String,
    pub service_version: String,
    /// Tenant and device. Both are machine facts and both are explicitly acceptable
    /// telemetry attributes: the gateway already knows them from the device
    /// certificate, and without them a fleet-wide telemetry stream cannot be narrowed
    /// to the floor that is having the problem.
    pub tenant_id: Option<String>,
    pub device_id: Option<String>,
}

impl Resource {
    fn to_json(&self) -> Value {
        let mut attrs = vec![
            attr("service.name", &AttrValue::Str(self.service_name.clone())),
            attr("service.version", &AttrValue::Str(self.service_version.clone())),
        ];
        if let Some(t) = &self.tenant_id {
            attrs.push(attr("tenant.id", &AttrValue::Str(t.clone())));
        }
        if let Some(d) = &self.device_id {
            attrs.push(attr("device.id", &AttrValue::Str(d.clone())));
        }
        json!({"attributes": attrs})
    }
}

fn attr(key: &str, value: &AttrValue) -> Value {
    let mut m = Map::new();
    m.insert("key".into(), Value::String(key.to_string()));
    m.insert("value".into(), value.to_any_value());
    Value::Object(m)
}

/// Encode a batch as an `ExportLogsServiceRequest`.
pub fn encode(resource: &Resource, records: &[Record]) -> Vec<u8> {
    let log_records: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "timeUnixNano": r.time_unix_nano.to_string(),
                "observedTimeUnixNano": r.time_unix_nano.to_string(),
                "severityNumber": r.severity as i32,
                "severityText": r.severity.text(),
                "body": {"stringValue": r.body},
                "attributes": r.attributes.iter().map(|(k, v)| attr(k, v)).collect::<Vec<_>>(),
            })
        })
        .collect();

    let body = json!({
        "resourceLogs": [{
            "resource": resource.to_json(),
            "scopeLogs": [{
                "scope": {
                    "name": resource.service_name,
                    "version": resource.service_version,
                },
                "logRecords": log_records,
            }],
        }],
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource() -> Resource {
        Resource {
            service_name: "sentinel-agent".into(),
            service_version: "0.1.0".into(),
            tenant_id: Some("t-acme".into()),
            device_id: Some("1b4e28ba".into()),
        }
    }

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("valid JSON")
    }

    #[test]
    fn the_envelope_is_an_export_logs_service_request() {
        let v = parse(&encode(&resource(), &[]));
        let scope_logs = &v["resourceLogs"][0]["scopeLogs"][0];
        assert_eq!(scope_logs["scope"]["name"], "sentinel-agent");
        assert_eq!(scope_logs["logRecords"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_service_name_is_the_one_the_requirement_names() {
        for name in ["sentinel-agent", "sentinel-service"] {
            let r = Resource { service_name: name.into(), ..resource() };
            let v = parse(&encode(&r, &[]));
            let attrs = v["resourceLogs"][0]["resource"]["attributes"].as_array().unwrap().clone();
            let found = attrs
                .iter()
                .find(|a| a["key"] == "service.name")
                .expect("service.name is required by the OTel resource semantic conventions");
            assert_eq!(found["value"]["stringValue"], name);
        }
    }

    #[test]
    fn tenant_and_device_travel_on_the_resource_and_are_omitted_when_unknown() {
        let v = parse(&encode(&resource(), &[]));
        let attrs = v["resourceLogs"][0]["resource"]["attributes"].as_array().unwrap().clone();
        let keys: Vec<&str> = attrs.iter().map(|a| a["key"].as_str().unwrap()).collect();
        assert!(keys.contains(&"tenant.id"));
        assert!(keys.contains(&"device.id"));

        // A machine that has not enrolled yet knows neither, and must not send an
        // empty string that a dashboard will group everything under.
        let bare = Resource { tenant_id: None, device_id: None, ..resource() };
        let v = parse(&encode(&bare, &[]));
        let attrs = v["resourceLogs"][0]["resource"]["attributes"].as_array().unwrap().clone();
        let keys: Vec<&str> = attrs.iter().map(|a| a["key"].as_str().unwrap()).collect();
        assert_eq!(keys, vec!["service.name", "service.version"]);
    }

    #[test]
    fn sixtyfour_bit_values_are_encoded_as_strings() {
        // JSON numbers are doubles. A nanosecond timestamp is ~1.8e18 and loses its
        // last two digits; collectors reject or mangle a numeric one.
        let record = Record {
            time_unix_nano: 1_788_000_000_123_456_789,
            severity: Severity::Info,
            body: "capture armed".into(),
            attributes: vec![("spool.depth".into(), AttrValue::Int(9_007_199_254_740_993))],
        };
        let v = parse(&encode(&resource(), &[record]));
        let lr = &v["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(lr["timeUnixNano"], "1788000000123456789");
        assert_eq!(lr["observedTimeUnixNano"], "1788000000123456789");
        assert_eq!(lr["attributes"][0]["value"]["intValue"], "9007199254740993");
    }

    #[test]
    fn severities_use_the_opentelemetry_numbers() {
        assert_eq!(Severity::from_tracing(&tracing::Level::TRACE) as i32, 1);
        assert_eq!(Severity::from_tracing(&tracing::Level::DEBUG) as i32, 5);
        assert_eq!(Severity::from_tracing(&tracing::Level::INFO) as i32, 9);
        assert_eq!(Severity::from_tracing(&tracing::Level::WARN) as i32, 13);
        assert_eq!(Severity::from_tracing(&tracing::Level::ERROR) as i32, 17);
        assert_eq!(Severity::Error.text(), "ERROR");
    }

    #[test]
    fn the_body_is_the_message_and_fields_are_attributes() {
        let record = Record {
            time_unix_nano: 1,
            severity: Severity::Warn,
            body: "capture could not arm".into(),
            attributes: vec![
                ("reason".into(), AttrValue::Str("pinned_device_missing".into())),
                ("armed".into(), AttrValue::Bool(false)),
                ("ack.lag_ms".into(), AttrValue::Float(1234.5)),
            ],
        };
        let v = parse(&encode(&resource(), &[record]));
        let lr = &v["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(lr["body"]["stringValue"], "capture could not arm");
        assert_eq!(lr["severityText"], "WARN");
        assert_eq!(lr["attributes"][0]["value"]["stringValue"], "pinned_device_missing");
        assert_eq!(lr["attributes"][1]["value"]["boolValue"], false);
        assert_eq!(lr["attributes"][2]["value"]["doubleValue"], 1234.5);
    }

    #[test]
    fn a_batch_carries_every_record_in_order() {
        let records: Vec<Record> = (0..5)
            .map(|i| Record {
                time_unix_nano: i as u128,
                severity: Severity::Info,
                body: format!("event {i}"),
                attributes: vec![],
            })
            .collect();
        let v = parse(&encode(&resource(), &records));
        let list = v["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array().unwrap().clone();
        assert_eq!(list.len(), 5);
        for (i, lr) in list.iter().enumerate() {
            assert_eq!(lr["body"]["stringValue"], format!("event {i}"));
        }
    }
}
