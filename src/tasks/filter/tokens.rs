//! Filter expression AST and token types.

use chrono::{DateTime, Utc};

/// A parsed filter expression tree.
#[derive(Debug, Clone)]
pub enum FilterExpr {
    /// Attribute comparison: name.modifier:value
    Attribute {
        name: String,
        modifier: AttrModifier,
        value: String,
        /// Pre-parsed date value for date attributes (due, entry, modified, wait, scheduled).
        /// Avoids re-parsing the date string for every task during evaluation.
        parsed_date: Option<DateTime<Utc>>,
    },
    /// Tag presence: +tag
    HasTag(String),
    /// Tag absence: -tag
    NotTag(String),
    /// Boolean AND
    And(Box<FilterExpr>, Box<FilterExpr>),
    /// Boolean OR
    Or(Box<FilterExpr>, Box<FilterExpr>),
    /// Boolean NOT
    Not(Box<FilterExpr>),
    /// Always true (empty filter)
    True,
}

/// Known date attribute names — values for these are pre-parsed during filter parsing.
pub const DATE_ATTRIBUTES: &[&str] = &["due", "entry", "modified", "wait", "scheduled"];

/// Attribute comparison modifiers.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrModifier {
    /// Default: approximate match (left-match for strings, same-day for dates)
    Equals,
    /// Exact equality
    Is,
    /// Not equal
    Isnt,
    /// Less than (before)
    Before,
    /// Greater than (after)
    After,
    /// Less than or equal (by)
    By,
    /// Contains / regex match
    Has,
    /// Does not contain
    Hasnt,
    /// Starts with
    StartsWith,
    /// Ends with
    EndsWith,
    /// Attribute has no value
    None,
    /// Attribute has any value
    Any,
}
