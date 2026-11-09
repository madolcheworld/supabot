//! Settings: the brain's behaviour, read through to the store.
//!
//! Every hot path (a turn, a tool call, a routing decision) needs the hop limit
//! or a ceiling, so this used to keep one cached copy per handle. That was
//! wrong, and the bug it produced was a safety bug: two `SettingsHandle`s could
//! disagree, so flipping the bot-messaging kill switch through one left the
//! other handing out delegations (see `docs/decisions.md` D9.4).
//!
//! One row read is cheap, so [`SettingsHandle::get`] reads the store and the
//! in-memory copy is only a fallback for when the store itself fails. `reload`
//! remains for callers that want the write to fail loudly, and `update` still
//! persists before it returns, so a change made through the API is visible to
//! every holder immediately. Do not "optimise" this back into a per-handle
//! cache without answering how two holders stay in agreement.

use std::sync::{Arc, Mutex};

use tracing::warn;

use swarmbot_contracts::config::Settings;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::repo::Persistence;

pub struct SettingsHandle {
    store: Arc<dyn Persistence>,
    cached: Mutex<Settings>,
}

impl SettingsHandle {
    pub fn new(store: Arc<dyn Persistence>) -> Arc<Self> {
        let initial = store.settings().unwrap_or_else(|err| {
            warn!(error = %err, "cannot read settings; using the defaults");
            Settings::default()
        });
        Arc::new(Self {
            store,
            cached: Mutex::new(initial),
        })
    }

    /// The current settings.
    ///
    /// Read straight from the store rather than from the cache: two holders of a
    /// `SettingsHandle` must never disagree, and a stale cache on a policy value
    /// (the bot-messaging kill switch, a ceiling, the hop limit) is a safety bug,
    /// not a performance win. One row read is cheap. The cache only backs the
    /// case where the store itself fails.
    pub fn get(&self) -> Settings {
        self.store.settings().unwrap_or_else(|err| {
            warn!(error = %err, "cannot read settings; using the cached copy");
            self.cached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
    }

    /// Re-read from the store (another process may have changed it).
    pub fn reload(&self) -> Result<Settings, DomainError> {
        let settings = self.store.settings()?;
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = settings.clone();
        Ok(settings)
    }

    /// Change, persist and publish in one step. Returns the new value.
    pub fn update(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings, DomainError> {
        let mut settings = self.get();
        change(&mut settings);
        self.store.put_settings(&settings)?;
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = settings.clone();
        Ok(settings)
    }

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use swarmbot_contracts::node::PolicyCeiling;

    #[test]
    fn the_cache_matches_the_store() {
        let store = testing::memory_store();
        let handle = SettingsHandle::new(store.clone());
        assert_eq!(handle.get(), Settings::default());
        assert_eq!(handle.get().max_hops, 8);
        assert!(handle.get().bot_messaging);
    }

    #[test]
    fn an_update_is_persisted_and_visible_at_once() {
        let store = testing::memory_store();
        let handle = SettingsHandle::new(store.clone());
        let updated = handle
            .update(|settings| {
                settings.bot_messaging = false;
                settings.max_hops = 3;
            })
            .unwrap();
        assert!(!updated.bot_messaging);
        assert_eq!(handle.get().max_hops, 3);
        assert_eq!(store.settings().unwrap().max_hops, 3);
    }

    #[test]
    fn a_change_from_elsewhere_is_visible_at_once() {
        let store = testing::memory_store();
        let handle = SettingsHandle::new(store.clone());
        let other = SettingsHandle::new(store.clone());
        let mut stored = store.settings().unwrap();
        stored.approval_expiry_secs = 30;
        store.put_settings(&stored).unwrap();

        assert_eq!(handle.get().approval_expiry_secs, 30, "no stale cache");
        assert_eq!(other.get().approval_expiry_secs, 30);
        assert_eq!(handle.reload().unwrap().approval_expiry_secs, 30);
    }

    #[test]
    fn two_handles_never_disagree_about_the_kill_switch() {
        let store = testing::memory_store();
        let a = SettingsHandle::new(store.clone());
        let b = SettingsHandle::new(store.clone());
        assert!(b.get().bot_messaging);
        a.update(|settings| settings.bot_messaging = false).unwrap();
        assert!(
            !b.get().bot_messaging,
            "the kill switch takes effect everywhere"
        );
    }

    #[test]
    fn ceilings_are_reachable_from_the_handle() {
        let handle = SettingsHandle::new(testing::memory_store());
        handle
            .update(|settings| {
                settings.safety.set(
                    swarmbot_contracts::node::Surface::Shell,
                    PolicyCeiling::Always,
                )
            })
            .unwrap();
        assert_eq!(
            handle
                .get()
                .safety
                .get(swarmbot_contracts::node::Surface::Shell),
            PolicyCeiling::Always
        );
    }
}
