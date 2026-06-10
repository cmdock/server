use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

#[derive(Clone)]
pub struct RuntimeSyncCoordinator {
    bridge_freshness: BridgeFreshnessTracker,
}

impl RuntimeSyncCoordinator {
    pub fn new() -> Self {
        Self {
            bridge_freshness: BridgeFreshnessTracker::new(),
        }
    }

    pub fn note_canonical_change(&self, user_id: &str, _source: &'static str) {
        // MergedSyncGateway is the serving `/v1/client/*` runtime path. REST
        // writes only need to mark TC devices stale so their next read triggers
        // gateway projection; do not schedule the legacy sync-bridge writer here
        // or REST mutations would keep the non-serving `users/<id>/sync.sqlite`
        // chain warm after cutover.
        self.bridge_freshness.mark_canonical_changed(user_id);
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
}

impl Default for RuntimeSyncCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct BridgeFreshnessTracker {
    users: Arc<DashMap<String, Arc<UserFreshnessState>>>,
}

struct UserFreshnessState {
    canonical_generation: AtomicU64,
    device_generations: DashMap<String, u64>,
}

impl UserFreshnessState {
    fn new() -> Self {
        Self {
            // Start at 1 so newly registered devices are treated as stale until
            // they complete an initial gateway projection/read.
            canonical_generation: AtomicU64::new(1),
            device_generations: DashMap::new(),
        }
    }
}

impl BridgeFreshnessTracker {
    pub fn new() -> Self {
        Self {
            users: Arc::new(DashMap::new()),
        }
    }

    fn user_state(&self, user_id: &str) -> Arc<UserFreshnessState> {
        self.users
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(UserFreshnessState::new()))
            .clone()
    }

    pub fn mark_canonical_changed(&self, user_id: &str) -> u64 {
        self.user_state(user_id)
            .canonical_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1
    }

    pub fn mark_device_synced_to_current(&self, user_id: &str, client_id: &str) -> u64 {
        let state = self.user_state(user_id);
        let generation = state.canonical_generation.load(Ordering::Acquire);
        state
            .device_generations
            .insert(client_id.to_string(), generation);
        generation
    }

    pub fn mark_devices_synced_to_current<'a, I>(&self, user_id: &str, client_ids: I) -> u64
    where
        I: IntoIterator<Item = &'a str>,
    {
        let state = self.user_state(user_id);
        let generation = state.canonical_generation.load(Ordering::Acquire);
        for client_id in client_ids {
            state
                .device_generations
                .insert(client_id.to_string(), generation);
        }
        generation
    }

    pub fn mark_canonical_changed_and_device_synced(&self, user_id: &str, client_id: &str) -> u64 {
        let state = self.user_state(user_id);
        let generation = state.canonical_generation.fetch_add(1, Ordering::AcqRel) + 1;
        state
            .device_generations
            .insert(client_id.to_string(), generation);
        generation
    }

    pub fn device_needs_sync(&self, user_id: &str, client_id: &str) -> bool {
        let state = self.user_state(user_id);
        let canonical = state.canonical_generation.load(Ordering::Acquire);
        state
            .device_generations
            .get(client_id)
            .map(|seen| *seen < canonical)
            .unwrap_or(true)
    }

    pub fn remove_device(&self, user_id: &str, client_id: &str) {
        if let Some(state) = self.users.get(user_id) {
            state.device_generations.remove(client_id);
        }
    }

    pub fn clear_user(&self, user_id: &str) {
        self.users.remove(user_id);
    }
}

impl Default for BridgeFreshnessTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::BridgeFreshnessTracker;

    #[test]
    fn new_devices_start_stale_until_initial_sync() {
        let tracker = BridgeFreshnessTracker::new();

        assert!(tracker.device_needs_sync("user-a", "device-a"));

        tracker.mark_device_synced_to_current("user-a", "device-a");
        assert!(!tracker.device_needs_sync("user-a", "device-a"));
    }

    #[test]
    fn canonical_changes_stale_other_devices_but_not_synced_one() {
        let tracker = BridgeFreshnessTracker::new();

        tracker.mark_device_synced_to_current("user-a", "device-a");
        tracker.mark_device_synced_to_current("user-a", "device-b");
        tracker.mark_canonical_changed_and_device_synced("user-a", "device-a");

        assert!(!tracker.device_needs_sync("user-a", "device-a"));
        assert!(tracker.device_needs_sync("user-a", "device-b"));

        tracker.mark_device_synced_to_current("user-a", "device-b");
        assert!(!tracker.device_needs_sync("user-a", "device-b"));
    }
}
