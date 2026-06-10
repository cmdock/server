//! Taskwarrior filter expression engine.
//!
//! Parses and evaluates Taskwarrior filter syntax against TaskChampion tasks.
//! Supports attribute filters, tag filters, virtual tags, named dates,
//! attribute modifiers, and boolean operators.

mod eval;
mod parse;
mod tokens;

pub use eval::{matches_filter, matches_parsed, matches_with_context, EvalCtx};
pub use parse::{parse_filter, parse_filter_at};
pub use tokens::FilterExpr;
