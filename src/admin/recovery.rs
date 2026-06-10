pub use crate::recovery::{
    assess_user_recovery, RecoveryStatus, StartupRecoverySummary, UserRecoveryAssessment,
};

use crate::app_state::AppState;

/// Run the startup recovery assessment using the operator-side
/// `RecoveryCoordinator`. This wrapper lives in `admin/` because it
/// composes admin operator services on top of the core `recovery.rs`
/// assessment primitive — the dependency direction is admin → recovery,
/// never the reverse (per ADR-0002 §Independence).
pub async fn run_startup_recovery_assessment(
    state: &AppState,
) -> anyhow::Result<StartupRecoverySummary> {
    crate::admin::services::recovery::RecoveryCoordinator::for_running_state(state)
        .startup_assessment()
        .await
}
