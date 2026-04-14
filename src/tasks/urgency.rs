//! Urgency calculation for tasks.
//!
//! Implements the Taskwarrior urgency algorithm.

use chrono::{DateTime, Utc};

/// Calculate urgency score for a task based on its attributes.
///
/// Mirrors Taskwarrior's urgency calculation:
/// - Priority: H=6.0, M=3.9, L=1.8
/// - Due date: scales from -12.0 (far future) to 12.0 (overdue)
/// - Tags: +1.0 per tag (max 3.0)
/// - Project: +1.0 if set
pub fn calculate_urgency(
    priority: Option<&str>,
    due: Option<DateTime<Utc>>,
    tag_count: usize,
    has_project: bool,
) -> f64 {
    let mut urgency = 0.0;

    // Priority coefficient
    urgency += match priority {
        Some("H") => 6.0,
        Some("M") => 3.9,
        Some("L") => 1.8,
        _ => 0.0,
    };

    // Due date coefficient (-12.0 to 12.0)
    if let Some(due_dt) = due {
        let now = Utc::now();
        let days_until = (due_dt - now).num_seconds() as f64 / 86400.0;

        urgency += if days_until < -14.0 {
            12.0 // Very overdue
        } else if days_until < 0.0 {
            // Overdue: scale from 12.0 to 8.0
            12.0 - (days_until.abs() / 14.0) * 4.0
        } else if days_until < 7.0 {
            // Due within a week: scale from 8.0 to 0.0
            8.0 * (1.0 - days_until / 7.0)
        } else if days_until < 14.0 {
            // Due within two weeks: scale from 0.0 to -4.0
            -4.0 * (days_until - 7.0) / 7.0
        } else {
            -12.0 // Far future
        };
    }

    // Tag coefficient (max 3 tags contribute)
    urgency += (tag_count.min(3) as f64) * 1.0;

    // Project coefficient
    if has_project {
        urgency += 1.0;
    }

    urgency
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_high_priority() {
        let u = calculate_urgency(Some("H"), None, 0, false);
        assert!((u - 6.0).abs() < 0.01);
    }

    #[test]
    fn test_with_project_and_tags() {
        let u = calculate_urgency(None, None, 2, true);
        assert!((u - 3.0).abs() < 0.01); // 2 tags + 1 project
    }

    #[test]
    fn test_overdue() {
        let past = Utc::now() - chrono::Duration::days(1);
        let u = calculate_urgency(None, Some(past), 0, false);
        assert!(u > 8.0); // Overdue should be high urgency
    }

    #[test]
    fn test_far_future() {
        let future = Utc::now() + chrono::Duration::days(30);
        let u = calculate_urgency(None, Some(future), 0, false);
        assert!(u < 0.0); // Far future should be negative urgency
    }
}
