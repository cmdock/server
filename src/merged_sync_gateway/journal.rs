//! Typed forward-only journal states for the merged sync gateway.
//!
//! The store layer owns persistence/SQL. This module owns the state vocabulary
//! and transition rules used by gateway planner/recovery code.

use serde::{Deserialize, Serialize};

/// Durable gateway journal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayJournalState {
    Received,
    MergedVersionAccepted,
    SourcePlanApplied,
    ProjectionAppended,
    Finalized,
    Failed,
    Quarantined,
}

impl GatewayJournalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::MergedVersionAccepted => "merged_version_accepted",
            Self::SourcePlanApplied => "source_plan_applied",
            Self::ProjectionAppended => "projection_appended",
            Self::Finalized => "finalized",
            Self::Failed => "failed",
            Self::Quarantined => "quarantined",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "received" => Some(Self::Received),
            "merged_version_accepted" => Some(Self::MergedVersionAccepted),
            "source_plan_applied" => Some(Self::SourcePlanApplied),
            "projection_appended" => Some(Self::ProjectionAppended),
            "finalized" => Some(Self::Finalized),
            "failed" => Some(Self::Failed),
            "quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finalized | Self::Failed | Self::Quarantined)
    }

    /// Forward-only transition rule for ordinary gateway processing and
    /// recovery. Recovery may move any non-terminal state to a diagnostic
    /// terminal state, but it may not rewrite a terminal row.
    pub fn can_transition_to(self, next: Self) -> bool {
        use GatewayJournalState as S;
        matches!(
            (self, next),
            (S::Received, S::MergedVersionAccepted)
                | (S::MergedVersionAccepted, S::SourcePlanApplied)
                | (S::SourcePlanApplied, S::ProjectionAppended)
                | (S::ProjectionAppended, S::Finalized)
                | (S::Received, S::Failed)
                | (S::Received, S::Quarantined)
                | (S::MergedVersionAccepted, S::Failed)
                | (S::MergedVersionAccepted, S::Quarantined)
                | (S::SourcePlanApplied, S::Failed)
                | (S::SourcePlanApplied, S::Quarantined)
                | (S::ProjectionAppended, S::Failed)
                | (S::ProjectionAppended, S::Quarantined)
        )
    }
}

/// Operator-facing recovery state for a journal row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayRecoveryStatus {
    NotRequired,
    Recoverable,
    Recovered,
    Failed,
    Quarantined,
}

impl GatewayRecoveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Recoverable => "recoverable",
            Self::Recovered => "recovered",
            Self::Failed => "failed",
            Self::Quarantined => "quarantined",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "not_required" => Some(Self::NotRequired),
            "recoverable" => Some(Self::Recoverable),
            "recovered" => Some(Self::Recovered),
            "failed" => Some(Self::Failed),
            "quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_journal_transitions_are_forward_only() {
        assert!(GatewayJournalState::Received
            .can_transition_to(GatewayJournalState::MergedVersionAccepted));
        assert!(GatewayJournalState::MergedVersionAccepted
            .can_transition_to(GatewayJournalState::SourcePlanApplied));
        assert!(GatewayJournalState::SourcePlanApplied
            .can_transition_to(GatewayJournalState::ProjectionAppended));
        assert!(GatewayJournalState::ProjectionAppended
            .can_transition_to(GatewayJournalState::Finalized));

        assert!(!GatewayJournalState::SourcePlanApplied
            .can_transition_to(GatewayJournalState::MergedVersionAccepted));
        assert!(!GatewayJournalState::Finalized.can_transition_to(GatewayJournalState::Failed));
    }

    #[test]
    fn non_terminal_states_can_enter_diagnostic_terminal_states() {
        for state in [
            GatewayJournalState::Received,
            GatewayJournalState::MergedVersionAccepted,
            GatewayJournalState::SourcePlanApplied,
            GatewayJournalState::ProjectionAppended,
        ] {
            assert!(state.can_transition_to(GatewayJournalState::Failed));
            assert!(state.can_transition_to(GatewayJournalState::Quarantined));
        }
    }
}
