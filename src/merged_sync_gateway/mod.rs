//! Merged TaskChampion sync gateway.
//!
//! This module is the ADR-0009 boundary for TW sync. It will own the
//! forward-only orchestration from an inbound merged-chain TC version to
//! canonical source-replica writes and merged-chain projection.
//!
//! The first production slice is the codec boundary: raw TaskChampion
//! history-segment JSON is decoded here into typed `WireOp` values and must not
//! leak into protocol handlers or source-write code.

mod audit;
pub mod codec;
pub mod inbound;
pub mod journal;
mod journal_ops;
mod planner;
pub mod projection;
pub mod protocol;
pub mod recovery;
mod recovery_acceptance;
mod source;
mod sqlite_error;
pub mod storage;
