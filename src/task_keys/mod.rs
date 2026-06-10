//! Task-key allocation runtime — coordinator-side logic for the
//! `<PREFIX>-N` user-facing keys defined in `task-write-contract.md`
//! § Task Keys.
//!
//! DB primitives (reservation, commit, burn, lookup) live in
//! `src/store/sqlite/task_keys.rs`. This module owns AppState-coupled
//! coordination — the reaper (per-user lock + decision model), the
//! Phase 4 personal Task Scope lazy backfill, and (Phase 5) the sync-bridge
//! drift recovery pass.

pub mod backfill;
pub mod drift;
pub mod reaper;
pub(crate) mod udas;

pub use reaper::{run_reaper_pass, ReaperOutcome};
