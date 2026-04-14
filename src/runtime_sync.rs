use crate::app_state::AppState;
use crate::sync_bridge::{BridgeFreshnessTracker, BridgeScheduler, SyncPriority};

#[derive(Clone)]
pub struct RuntimeSyncCoordinator {
    bridge_scheduler: BridgeScheduler,
    bridge_freshness: BridgeFreshnessTracker,
}

impl RuntimeSyncCoordinator {
    pub fn new() -> Self {
        Self {
            bridge_scheduler: BridgeScheduler::new(),
            bridge_freshness: BridgeFreshnessTracker::new(),
        }
    }

    pub fn start(&self, state: &AppState) {
        self.bridge_scheduler.start(state);
    }

    pub fn note_canonical_change(&self, user_id: &str, source: &'static str) {
        self.bridge_freshness.mark_canonical_changed(user_id);
        self.bridge_scheduler
            .schedule(user_id, SyncPriority::Normal, source);
    }

    pub fn schedule(&self, user_id: &str, priority: SyncPriority, source: &'static str) {
        self.bridge_scheduler.schedule(user_id, priority, source);
    }

    pub fn device_needs_sync(&self, user_id: &str, client_id: &str) -> bool {
        self.bridge_freshness.device_needs_sync(user_id, client_id)
    }

    pub fn mark_device_synced_to_current(&self, user_id: &str, client_id: &str) -> u64 {
        self.bridge_freshness
            .mark_device_synced_to_current(user_id, client_id)
    }

    pub fn mark_devices_synced_to_current<'a, I>(&self, user_id: &str, client_ids: I) -> u64
    where
        I: IntoIterator<Item = &'a str>,
    {
        self.bridge_freshness
            .mark_devices_synced_to_current(user_id, client_ids)
    }

    pub fn mark_canonical_changed_and_device_synced(&self, user_id: &str, client_id: &str) -> u64 {
        self.bridge_freshness
            .mark_canonical_changed_and_device_synced(user_id, client_id)
    }

    pub fn remove_device(&self, user_id: &str, client_id: &str) {
        self.bridge_freshness.remove_device(user_id, client_id);
    }

    pub fn clear_user(&self, user_id: &str) {
        self.bridge_freshness.clear_user(user_id);
    }

    pub fn freshness_tracker(&self) -> BridgeFreshnessTracker {
        self.bridge_freshness.clone()
    }

    pub fn scheduler(&self) -> BridgeScheduler {
        self.bridge_scheduler.clone()
    }
}

impl Default for RuntimeSyncCoordinator {
    fn default() -> Self {
        Self::new()
    }
}
