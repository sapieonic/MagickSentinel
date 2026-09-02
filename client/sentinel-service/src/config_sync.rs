//! Tenant config sync.
//!
//! The service holds the authoritative copy of `LocalConfig` (machine-scoped, written
//! by the installer) and the last `Policy` snapshot fetched from `GET /v1/policy`.
//! The agent asks for both over the pipe rather than reading `%PROGRAMDATA%` itself,
//! so there is exactly one writer and a shift change cannot race two agents against
//! one file.
//!
//! Only the caching is here. The fetch is a `PolicyFetch` implementation supplied by
//! the caller: `/v1/policy` is device-scoped and authenticated with the device
//! certificate, and wiring rustls into this module would make the cache untestable
//! for no gain.

use sentinel_core::config::{LocalConfig, Policy};
use std::path::{Path, PathBuf};

/// Fetches a policy snapshot. Implemented over mTLS in the service binary; faked in
/// tests.
pub trait PolicyFetch {
    fn fetch(&self) -> anyhow::Result<Policy>;
}

pub struct ConfigStore {
    dir: PathBuf,
    local: LocalConfig,
    policy: Option<Policy>,
}

impl ConfigStore {
    /// Load whatever is on disk. A missing or corrupt policy cache is not fatal — the
    /// agent stays blocked until a real policy arrives, which is the correct posture
    /// anyway: without a pinned device, capture must not start.
    pub fn open(dir: &Path) -> Self {
        let local = std::fs::read_to_string(dir.join("config.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<LocalConfig>(&s).ok())
            .unwrap_or_default();
        let policy = std::fs::read_to_string(dir.join("policy.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Policy>(&s).ok());
        ConfigStore { dir: dir.to_path_buf(), local, policy }
    }

    pub fn local(&self) -> &LocalConfig {
        &self.local
    }

    pub fn policy(&self) -> Option<&Policy> {
        self.policy.as_ref()
    }

    /// Refresh from the server, persisting only on success.
    ///
    /// A failed fetch keeps the cached policy rather than clearing it: the floor's
    /// internet drops regularly, and dropping the pinned-device configuration every
    /// time it does would stop capture for a reason that has nothing to do with
    /// identity. The offline-grace clock, not this cache, is what eventually stops
    /// capture.
    pub fn refresh(&mut self, fetcher: &dyn PolicyFetch) -> anyhow::Result<bool> {
        let fresh = fetcher.fetch()?;
        let changed = self.policy.as_ref() != Some(&fresh);
        if changed {
            self.persist_policy(&fresh)?;
            self.policy = Some(fresh);
        }
        Ok(changed)
    }

    fn persist_policy(&self, p: &Policy) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join("policy.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(p)?)?;
        std::fs::rename(tmp, self.dir.join("policy.json"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_core::config::PinnedDevice;
    use std::cell::RefCell;

    struct Fake(RefCell<Vec<anyhow::Result<Policy>>>);
    impl PolicyFetch for Fake {
        fn fetch(&self) -> anyhow::Result<Policy> {
            self.0.borrow_mut().remove(0)
        }
    }

    fn policy(version: i64) -> Policy {
        Policy {
            version,
            pinned_devices: vec![PinnedDevice {
                container_id: "cont-a".into(),
                friendly_name: Some("Jabra Evolve 20".into()),
            }],
            ..Policy::default()
        }
    }

    #[test]
    fn a_fetched_policy_is_cached_and_reloads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::open(dir.path());
        assert!(store.policy().is_none());

        let f = Fake(RefCell::new(vec![Ok(policy(7))]));
        assert!(store.refresh(&f).unwrap(), "first fetch is a change");
        assert_eq!(store.policy().unwrap().version, 7);

        let reopened = ConfigStore::open(dir.path());
        assert_eq!(reopened.policy().unwrap().version, 7);
    }

    #[test]
    fn an_unchanged_policy_is_not_reported_as_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::open(dir.path());
        let f = Fake(RefCell::new(vec![Ok(policy(7)), Ok(policy(7))]));
        assert!(store.refresh(&f).unwrap());
        assert!(!store.refresh(&f).unwrap());
    }

    #[test]
    fn a_failed_fetch_keeps_the_cached_policy() {
        // The floor's internet drops. Losing the pinned-device configuration on every
        // drop would stop capture for a reason unrelated to identity; the
        // offline-grace clock is what is allowed to stop capture.
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::open(dir.path());
        let f = Fake(RefCell::new(vec![Ok(policy(7)), Err(anyhow::anyhow!("dns"))]));
        store.refresh(&f).unwrap();
        assert!(store.refresh(&f).is_err());
        assert_eq!(store.policy().unwrap().version, 7);
        assert_eq!(store.policy().unwrap().pinned_devices.len(), 1);
    }

    #[test]
    fn a_corrupt_cache_leaves_the_agent_blocked_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("policy.json"), b"{ this is not json").unwrap();
        let store = ConfigStore::open(dir.path());
        assert!(store.policy().is_none(), "no policy means no pinned device means no capture");
        assert_eq!(store.local().api_base_url, LocalConfig::default().api_base_url);
    }
}
