//! Urgency calculation for tasks.
//!
//! Implements the stock Taskwarrior urgency algorithm with default coefficients.
//! See: <https://taskwarrior.org/docs/urgency/>

use chrono::{DateTime, Utc};

// Stock Taskwarrior default coefficients
const COEFF_PRIORITY: f64 = 6.0; // H=1.0, M=0.65, L=0.3
const COEFF_PROJECT: f64 = 1.0;
const COEFF_DUE: f64 = 12.0;
const COEFF_TAGS: f64 = 1.0;
const COEFF_ANNOTATIONS: f64 = 1.0;
const COEFF_AGE: f64 = 2.0;
const COEFF_ACTIVE: f64 = 4.0;
const COEFF_BLOCKING: f64 = 8.0;
const COEFF_SCHEDULED: f64 = 5.0;
const COEFF_BLOCKED: f64 = -5.0;
const COEFF_WAITING: f64 = -3.0;

/// Maximum age (in days) for the age urgency factor.
const AGE_MAX_DAYS: f64 = 365.0;

/// All inputs needed for urgency calculation.
///
/// `now` is accepted explicitly for deterministic testing (matches the filter
/// engine's threaded-`now` pattern).
pub struct UrgencyInputs<'a> {
    pub priority: Option<&'a str>,
    pub due: Option<DateTime<Utc>>,
    pub tag_count: usize,
    pub annotation_count: usize,
    pub has_project: bool,
    pub is_active: bool,
    pub is_blocked: bool,
    pub is_blocking: bool,
    pub scheduled: Option<DateTime<Utc>>,
    pub entry: Option<DateTime<Utc>>,
    pub is_waiting: bool,
    pub now: DateTime<Utc>,
}

/// Calculate urgency score for a task using stock Taskwarrior defaults.
pub fn calculate_urgency(i: &UrgencyInputs<'_>) -> f64 {
    let mut urgency = 0.0;

    // Priority: H=6.0, M=3.9, L=1.8
    urgency += COEFF_PRIORITY
        * match i.priority {
            Some("H") => 1.0,
            Some("M") => 0.65,
            Some("L") => 0.3,
            _ => 0.0,
        };

    // Due: piecewise linear, never negative.
    //   >= 7d overdue:           scaling = 1.0
    //   14d future → 7d overdue: scaling = linear 0.2 → 1.0 (21-day window)
    //   > 14d future:            scaling = 0.2
    //   no due date:             scaling = 0.0
    if let Some(due_dt) = i.due {
        let days_until = (due_dt - i.now).num_seconds() as f64 / 86400.0;
        let scaling = if days_until <= -7.0 {
            // 7+ days overdue — maximum urgency
            1.0
        } else if days_until >= 14.0 {
            // 14+ days in the future — minimum (but positive) urgency
            0.2
        } else {
            // Linear interpolation across the 21-day window:
            // days_until = -7  → scaling = 1.0
            // days_until = 14  → scaling = 0.2
            1.0 - ((days_until + 7.0) / 21.0) * 0.8
        };
        urgency += COEFF_DUE * scaling;
    }

    // Tags: stepped 0/0.8/0.9/1.0 for 0/1/2/>=3 tags
    urgency += COEFF_TAGS * count_scaling(i.tag_count);

    // Annotations: stepped 0/0.8/0.9/1.0 for 0/1/2/>=3 annotations
    urgency += COEFF_ANNOTATIONS * count_scaling(i.annotation_count);

    // Project: 1.0 if set
    if i.has_project {
        urgency += COEFF_PROJECT;
    }

    // Active (started): 4.0 if task has been started
    if i.is_active {
        urgency += COEFF_ACTIVE;
    }

    // Blocking: 8.0 if task has unresolved dependents
    if i.is_blocking {
        urgency += COEFF_BLOCKING;
    }

    // Scheduled: 5.0 only when past its scheduled date
    if let Some(sched) = i.scheduled {
        if sched < i.now {
            urgency += COEFF_SCHEDULED;
        }
    }

    // Age: linear 0→2.0 over 365 days, capped at 2.0
    if let Some(entry_dt) = i.entry {
        let age_days = (i.now - entry_dt).num_seconds() as f64 / 86400.0;
        if age_days > 0.0 {
            let scaling = (age_days / AGE_MAX_DAYS).min(1.0);
            urgency += COEFF_AGE * scaling;
        }
    }

    // Blocked: -5.0 if task depends on unresolved tasks
    if i.is_blocked {
        urgency += COEFF_BLOCKED;
    }

    // Waiting: -3.0 if task has a future wait date
    if i.is_waiting {
        urgency += COEFF_WAITING;
    }

    urgency
}

/// Compute urgency for a TaskChampion task.
///
/// Extracts all required attributes from the TC `Task` and feeds them into
/// [`calculate_urgency`]. This keeps TC-specific knowledge inside the tasks
/// module so callers (e.g. `replica::task_to_item`) don't need to understand
/// urgency inputs or TC storage encoding.
pub fn urgency_for_task(task: &taskchampion::Task, now: DateTime<Utc>) -> f64 {
    let priority_str = task.get_priority();
    calculate_urgency(&UrgencyInputs {
        priority: if priority_str.is_empty() {
            None
        } else {
            Some(priority_str)
        },
        due: task.get_due(),
        tag_count: task.get_tags().filter(|t| t.is_user()).count(),
        annotation_count: task.get_annotations().count(),
        has_project: task.get_value("project").is_some(),
        is_active: task.is_active(),
        is_blocked: task.is_blocked(),
        is_blocking: task.is_blocking(),
        scheduled: super::parse_task_scheduled(task),
        entry: task.get_entry(),
        is_waiting: task.get_wait().is_some_and(|wait| wait > now),
        now,
    })
}

/// Stepped scaling for count-based factors (tags, annotations).
/// Stock TW: 0→0.0, 1→0.8, 2→0.9, >=3→1.0
fn count_scaling(count: usize) -> f64 {
    match count {
        0 => 0.0,
        1 => 0.8,
        2 => 0.9,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn fixed_now() -> DateTime<Utc> {
        "2026-04-15T12:00:00Z".parse().unwrap()
    }

    /// Baseline inputs: all zeroed/false/None. Override fields under test.
    fn base() -> UrgencyInputs<'static> {
        UrgencyInputs {
            priority: None,
            due: None,
            tag_count: 0,
            annotation_count: 0,
            has_project: false,
            is_active: false,
            is_blocked: false,
            is_blocking: false,
            scheduled: None,
            entry: None,
            is_waiting: false,
            now: fixed_now(),
        }
    }

    // ── Priority ──────────────────────────────────────────────

    #[test]
    fn priority_high() {
        let u = calculate_urgency(&UrgencyInputs {
            priority: Some("H"),
            ..base()
        });
        assert!((u - 6.0).abs() < 0.001);
    }

    #[test]
    fn priority_medium() {
        let u = calculate_urgency(&UrgencyInputs {
            priority: Some("M"),
            ..base()
        });
        assert!((u - 3.9).abs() < 0.001);
    }

    #[test]
    fn priority_low() {
        let u = calculate_urgency(&UrgencyInputs {
            priority: Some("L"),
            ..base()
        });
        assert!((u - 1.8).abs() < 0.001);
    }

    #[test]
    fn priority_none() {
        let u = calculate_urgency(&base());
        assert!((u - 0.0).abs() < 0.001);
    }

    // ── Due date ──────────────────────────────────────────────

    #[test]
    fn due_very_overdue() {
        // 30 days overdue → scaling = 1.0, contribution = 12.0
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            due: Some(now - TimeDelta::days(30)),
            ..base()
        });
        assert!((u - 12.0).abs() < 0.001);
    }

    #[test]
    fn due_exactly_7d_overdue() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            due: Some(now - TimeDelta::days(7)),
            ..base()
        });
        assert!((u - 12.0).abs() < 0.001);
    }

    #[test]
    fn due_today() {
        // days_until = 0 → scaling = 1.0 - (7/21)*0.8 ≈ 0.7333 → 12.0 * 0.7333 ≈ 8.8
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            due: Some(now),
            ..base()
        });
        assert!((u - 8.8).abs() < 0.1);
    }

    #[test]
    fn due_exactly_14d_future() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            due: Some(now + TimeDelta::days(14)),
            ..base()
        });
        assert!((u - 2.4).abs() < 0.001);
    }

    #[test]
    fn due_far_future() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            due: Some(now + TimeDelta::days(60)),
            ..base()
        });
        assert!((u - 2.4).abs() < 0.001);
    }

    #[test]
    fn due_never_negative() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            due: Some(now + TimeDelta::days(365)),
            ..base()
        });
        assert!(
            u >= 2.4 - 0.001,
            "due urgency must never be negative, got {u}"
        );
    }

    #[test]
    fn due_midpoint() {
        // 3.5 days future → scaling = 1.0 - (10.5/21)*0.8 = 0.6 → 12.0 * 0.6 = 7.2
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            due: Some(now + TimeDelta::hours(84)),
            ..base()
        });
        assert!((u - 7.2).abs() < 0.1);
    }

    #[test]
    fn no_due_date() {
        let u = calculate_urgency(&base());
        assert!((u - 0.0).abs() < 0.001);
    }

    // ── Tags ──────────────────────────────────────────────────

    #[test]
    fn tags_stepped() {
        assert!(
            (calculate_urgency(&UrgencyInputs {
                tag_count: 0,
                ..base()
            }) - 0.0)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                tag_count: 1,
                ..base()
            }) - 0.8)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                tag_count: 2,
                ..base()
            }) - 0.9)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                tag_count: 3,
                ..base()
            }) - 1.0)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                tag_count: 5,
                ..base()
            }) - 1.0)
                .abs()
                < 0.001
        );
    }

    // ── Annotations ───────────────────────────────────────────

    #[test]
    fn annotations_stepped() {
        assert!(
            (calculate_urgency(&UrgencyInputs {
                annotation_count: 0,
                ..base()
            }) - 0.0)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                annotation_count: 1,
                ..base()
            }) - 0.8)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                annotation_count: 2,
                ..base()
            }) - 0.9)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                annotation_count: 3,
                ..base()
            }) - 1.0)
                .abs()
                < 0.001
        );
        assert!(
            (calculate_urgency(&UrgencyInputs {
                annotation_count: 5,
                ..base()
            }) - 1.0)
                .abs()
                < 0.001
        );
    }

    // ── Project ───────────────────────────────────────────────

    #[test]
    fn project_contributes_one() {
        let u = calculate_urgency(&UrgencyInputs {
            has_project: true,
            ..base()
        });
        assert!((u - 1.0).abs() < 0.001);
    }

    // ── Active ────────────────────────────────────────────────

    #[test]
    fn active_contributes_four() {
        let u = calculate_urgency(&UrgencyInputs {
            is_active: true,
            ..base()
        });
        assert!((u - 4.0).abs() < 0.001);
    }

    // ── Blocking ──────────────────────────────────────────────

    #[test]
    fn blocking_contributes_eight() {
        let u = calculate_urgency(&UrgencyInputs {
            is_blocking: true,
            ..base()
        });
        assert!((u - 8.0).abs() < 0.001);
    }

    // ── Blocked ───────────────────────────────────────────────

    #[test]
    fn blocked_penalty() {
        let u = calculate_urgency(&UrgencyInputs {
            is_blocked: true,
            ..base()
        });
        assert!((u - (-5.0)).abs() < 0.001);
    }

    // ── Waiting ───────────────────────────────────────────────

    #[test]
    fn waiting_penalty() {
        let u = calculate_urgency(&UrgencyInputs {
            is_waiting: true,
            ..base()
        });
        assert!((u - (-3.0)).abs() < 0.001);
    }

    // ── Scheduled ─────────────────────────────────────────────

    #[test]
    fn scheduled_past_contributes() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            scheduled: Some(now - TimeDelta::days(1)),
            ..base()
        });
        assert!((u - 5.0).abs() < 0.001);
    }

    #[test]
    fn scheduled_future_no_contribution() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            scheduled: Some(now + TimeDelta::days(1)),
            ..base()
        });
        assert!((u - 0.0).abs() < 0.001);
    }

    // ── Age ───────────────────────────────────────────────────

    #[test]
    fn age_half_year() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            entry: Some(now - TimeDelta::days(182)),
            ..base()
        });
        assert!((u - (2.0 * 182.0 / 365.0)).abs() < 0.05);
    }

    #[test]
    fn age_full_year() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            entry: Some(now - TimeDelta::days(365)),
            ..base()
        });
        assert!((u - 2.0).abs() < 0.001);
    }

    #[test]
    fn age_capped_at_max() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            entry: Some(now - TimeDelta::days(730)),
            ..base()
        });
        assert!((u - 2.0).abs() < 0.001);
    }

    #[test]
    fn age_future_entry_ignored() {
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            entry: Some(now + TimeDelta::days(1)),
            ..base()
        });
        assert!((u - 0.0).abs() < 0.001);
    }

    #[test]
    fn no_entry_no_age() {
        let u = calculate_urgency(&base());
        assert!((u - 0.0).abs() < 0.001);
    }

    // ── Composite ─────────────────────────────────────────────

    #[test]
    fn composite_high_urgency_task() {
        // Priority H (6.0) + 7d overdue (12.0) + 3 tags (1.0) + project (1.0) + active (4.0) = 24.0
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            priority: Some("H"),
            due: Some(now - TimeDelta::days(7)),
            tag_count: 3,
            has_project: true,
            is_active: true,
            ..base()
        });
        assert!((u - 24.0).abs() < 0.1);
    }

    #[test]
    fn composite_blocked_waiting() {
        // Blocked (-5.0) + waiting (-3.0) = -8.0
        let u = calculate_urgency(&UrgencyInputs {
            is_blocked: true,
            is_waiting: true,
            ..base()
        });
        assert!((u - (-8.0)).abs() < 0.001);
    }

    #[test]
    fn composite_all_positive_factors() {
        // Priority H (6.0) + due 7d overdue (12.0) + 3 tags (1.0) + 3 annotations (1.0)
        // + project (1.0) + active (4.0) + blocking (8.0) + scheduled past (5.0)
        // + age 365d (2.0) = 40.0
        let now = fixed_now();
        let u = calculate_urgency(&UrgencyInputs {
            priority: Some("H"),
            due: Some(now - TimeDelta::days(7)),
            tag_count: 3,
            annotation_count: 3,
            has_project: true,
            is_active: true,
            is_blocking: true,
            scheduled: Some(now - TimeDelta::days(1)),
            entry: Some(now - TimeDelta::days(365)),
            ..base()
        });
        assert!((u - 40.0).abs() < 0.1);
    }

    #[test]
    fn all_fields_empty() {
        let u = calculate_urgency(&base());
        assert!((u - 0.0).abs() < 0.001);
    }

    // ── count_scaling helper ──────────────────────────────────

    #[test]
    fn count_scaling_values() {
        assert!((count_scaling(0) - 0.0).abs() < 0.001);
        assert!((count_scaling(1) - 0.8).abs() < 0.001);
        assert!((count_scaling(2) - 0.9).abs() < 0.001);
        assert!((count_scaling(3) - 1.0).abs() < 0.001);
        assert!((count_scaling(10) - 1.0).abs() < 0.001);
    }
}
