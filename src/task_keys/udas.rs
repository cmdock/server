//! Server-owned Taskwarrior UDA names for task-key identity.
//!
//! `cmdock_key` and `cmdock_task_scope` are the canonical UDAs visible to
//! Taskwarrior sync clients. `cmdock_account` is a historical compatibility
//! spelling: its value was derived from the Task Scope key prefix, not a Hosted
//! Account, Organisation, or tenant identifier. The server no longer writes
//! `cmdock_account` (TSKEY-007/SG-011); it projects `cmdock_task_scope` only,
//! and inbound `cmdock_account` values from legacy clients are cleared via
//! corrective projection. The constant is retained so those legacy values can
//! still be recognised and cleared.

/// User-facing canonical task key mirrored into TC for CLI/sync clients.
pub(crate) const CMDOCK_KEY_UDA: &str = "cmdock_key";

/// Legacy compatibility UDA that once carried the Task Scope key prefix.
///
/// No longer written by the server (TSKEY-007/SG-011). Retained so inbound
/// `cmdock_account` values from legacy TW clients can be recognised and cleared
/// via corrective projection; `cmdock_task_scope` is the canonical projection.
pub(crate) const CMDOCK_ACCOUNT_UDA: &str = "cmdock_account";

/// Canonical documented UDA name for the Task Scope key prefix.
pub(crate) const CMDOCK_TASK_SCOPE_UDA: &str = "cmdock_task_scope";
