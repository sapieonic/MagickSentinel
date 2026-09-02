//! Audio endpoint identity and hot-plug events.
//!
//! Agents unplug USB headsets constantly. Losing a call to a replugged headset is not
//! acceptable, so device identity is by container ID (stable across reconnects and
//! across USB ports) rather than by the endpoint ID, which changes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Microphone capture — the near channel.
    Capture,
    /// Render loopback — the far channel.
    Render,
}

/// A device identifier as it appears in tenant policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl From<&str> for DeviceId {
    fn from(s: &str) -> Self {
        DeviceId(s.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: DeviceId,
    /// `PKEY_Device_ContainerId`. Stable across replug; this is what policy matches on.
    pub container_id: Option<String>,
    pub friendly_name: String,
    pub direction: Direction,
    pub is_default: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Added(AudioDevice),
    Removed(DeviceId),
    StateChanged { id: DeviceId, active: bool },
    DefaultChanged { id: DeviceId, direction: Direction },
}

/// Resolve the pinned endpoint from tenant policy against what is actually present.
///
/// Container ID first, friendly name as the fallback, and nothing else — in
/// particular never the system default. On Tier B the default device would capture
/// whatever else the machine is playing, which is the privacy problem the pinning
/// requirement exists to prevent.
pub fn resolve_pinned<'a>(
    pinned: &[sentinel_core::config::PinnedDevice],
    available: &'a [AudioDevice],
    direction: Direction,
) -> Option<&'a AudioDevice> {
    for want in pinned {
        if let Some(dev) = available.iter().find(|d| {
            d.direction == direction
                && d.active
                && d.container_id.as_deref() == Some(want.container_id.as_str())
        }) {
            return Some(dev);
        }
    }
    for want in pinned {
        let Some(name) = want.friendly_name.as_deref() else { continue };
        if let Some(dev) = available
            .iter()
            .find(|d| d.direction == direction && d.active && d.friendly_name == name)
        {
            return Some(dev);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_core::config::PinnedDevice;

    fn dev(id: &str, container: Option<&str>, name: &str, dir: Direction, active: bool) -> AudioDevice {
        AudioDevice {
            id: DeviceId(id.into()),
            container_id: container.map(str::to_string),
            friendly_name: name.into(),
            direction: dir,
            is_default: false,
            active,
        }
    }

    fn pinned(container: &str, name: Option<&str>) -> PinnedDevice {
        PinnedDevice { container_id: container.into(), friendly_name: name.map(str::to_string) }
    }

    #[test]
    fn container_id_wins_over_friendly_name() {
        let available = vec![
            dev("ep-1", Some("cont-a"), "Jabra Evolve 20", Direction::Capture, true),
            dev("ep-2", Some("cont-b"), "Jabra Evolve 20", Direction::Capture, true),
        ];
        let got = resolve_pinned(&[pinned("cont-b", Some("Jabra Evolve 20"))], &available, Direction::Capture);
        assert_eq!(got.unwrap().id.0, "ep-2");
    }

    #[test]
    fn friendly_name_is_the_fallback_when_the_container_moved() {
        let available = vec![dev("ep-9", Some("cont-new"), "Plantronics C3220", Direction::Capture, true)];
        let got = resolve_pinned(&[pinned("cont-old", Some("Plantronics C3220"))], &available, Direction::Capture);
        assert_eq!(got.unwrap().id.0, "ep-9");
    }

    #[test]
    fn an_inactive_endpoint_is_not_a_match() {
        let available = vec![dev("ep-1", Some("cont-a"), "Jabra", Direction::Capture, false)];
        assert!(resolve_pinned(&[pinned("cont-a", None)], &available, Direction::Capture).is_none());
    }

    #[test]
    fn the_system_default_is_never_a_fallback() {
        // A default device present but unpinned must not be selected: on tier B it
        // would capture Spotify, Teams and notification sounds.
        let mut d = dev("ep-default", Some("cont-z"), "Speakers", Direction::Render, true);
        d.is_default = true;
        assert!(resolve_pinned(&[pinned("cont-a", Some("Headset"))], &[d], Direction::Render).is_none());
    }

    #[test]
    fn direction_is_respected() {
        let available = vec![dev("ep-1", Some("cont-a"), "Jabra", Direction::Render, true)];
        assert!(resolve_pinned(&[pinned("cont-a", None)], &available, Direction::Capture).is_none());
        assert!(resolve_pinned(&[pinned("cont-a", None)], &available, Direction::Render).is_some());
    }
}
