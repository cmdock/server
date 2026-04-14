//! Filter expression evaluator.
//!
//! Evaluates a FilterExpr AST against a TaskChampion Task.
//! Leverages TaskChampion's built-in `has_tag()` for the 8 synthetic tags
//! it supports (PENDING, COMPLETED, DELETED, WAITING, ACTIVE, BLOCKED,
//! UNBLOCKED, BLOCKING) and implements additional virtual tags ourselves.
//!
//! Performance: `now` is computed once per `matches_filter`/`matches_parsed`
//! call and threaded through all evaluation — not per-task. String comparisons
//! use `eq_ignore_ascii_case` to avoid per-task heap allocation.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use taskchampion::{Status, Tag, Task};

use super::dates::parse_date_value;
use super::parse::parse_filter;
use super::tokens::{AttrModifier, FilterExpr};

/// Snapshot of "now" for consistent evaluation across a batch of tasks.
struct EvalContext {
    now: DateTime<Utc>,
    today: NaiveDate,
}

impl EvalContext {
    fn new() -> Self {
        let now = Utc::now();
        Self {
            today: now.date_naive(),
            now,
        }
    }
}

/// Check whether a task matches a filter expression string.
/// NOTE: For hot loops, use `parse_filter` once and `matches_parsed` per task.
pub fn matches_filter(task: &Task, filter: &str) -> bool {
    let expr = parse_filter(filter);
    let ctx = EvalContext::new();
    eval_expr(task, &expr, &ctx)
}

/// Check whether a task matches a pre-parsed filter expression.
/// Use this in loops to avoid re-parsing the filter for every task.
pub fn matches_parsed(task: &Task, expr: &FilterExpr) -> bool {
    let ctx = EvalContext::new();
    eval_expr(task, expr, &ctx)
}

/// Check whether a task matches a pre-parsed filter with a shared context.
/// Use this to avoid even the Utc::now() call per task in hot loops.
/// `today` is read directly from the context — truly computed once per batch.
pub fn matches_with_context(task: &Task, expr: &FilterExpr, ctx: &EvalCtx) -> bool {
    let internal = EvalContext {
        now: ctx.now,
        today: ctx.today,
    };
    eval_expr(task, expr, &internal)
}

/// Public evaluation context — compute once, pass to `matches_with_context` per task.
/// Stores both `now` and `today` so neither is recomputed per task.
pub struct EvalCtx {
    pub now: DateTime<Utc>,
    pub today: NaiveDate,
}

impl EvalCtx {
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            today: now.date_naive(),
            now,
        }
    }
}

impl Default for EvalCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Evaluate a FilterExpr against a Task.
fn eval_expr(task: &Task, expr: &FilterExpr, ctx: &EvalContext) -> bool {
    match expr {
        FilterExpr::True => true,
        FilterExpr::And(left, right) => eval_expr(task, left, ctx) && eval_expr(task, right, ctx),
        FilterExpr::Or(left, right) => eval_expr(task, left, ctx) || eval_expr(task, right, ctx),
        FilterExpr::Not(inner) => !eval_expr(task, inner, ctx),
        FilterExpr::HasTag(tag) => eval_has_tag(task, tag, ctx),
        FilterExpr::NotTag(tag) => !eval_has_tag(task, tag, ctx),
        FilterExpr::Attribute {
            name,
            modifier,
            value,
            parsed_date,
        } => eval_attribute(task, name, modifier, value, *parsed_date, ctx),
    }
}

/// Check if a task has a tag (user or virtual).
fn eval_has_tag(task: &Task, tag_name: &str, ctx: &EvalContext) -> bool {
    if let Ok(tag) = Tag::try_from(tag_name) {
        return task.has_tag(&tag);
    }
    eval_virtual_tag(task, tag_name, ctx)
}

/// Evaluate virtual tags not built into TaskChampion.
/// Uses `eq_ignore_ascii_case` to avoid per-task allocation from to_uppercase().
fn eval_virtual_tag(task: &Task, tag_name: &str, ctx: &EvalContext) -> bool {
    let today = ctx.today;
    let now = ctx.now;
    let is_actionable =
        task.get_status() != Status::Completed && task.get_status() != Status::Deleted;

    if tag_name.eq_ignore_ascii_case("OVERDUE") {
        return is_actionable && task.get_due().is_some_and(|due| due.date_naive() < today);
    }
    if tag_name.eq_ignore_ascii_case("DUETODAY") || tag_name.eq_ignore_ascii_case("TODAY") {
        return is_actionable && task.get_due().is_some_and(|due| due.date_naive() == today);
    }
    if tag_name.eq_ignore_ascii_case("DUE") {
        return is_actionable
            && task
                .get_due()
                .is_some_and(|due| (due.date_naive() - today).num_days() <= 7);
    }
    if tag_name.eq_ignore_ascii_case("TOMORROW") {
        return is_actionable
            && task
                .get_due()
                .is_some_and(|due| due.date_naive() == today + Duration::days(1));
    }
    if tag_name.eq_ignore_ascii_case("YESTERDAY") {
        return is_actionable
            && task
                .get_due()
                .is_some_and(|due| due.date_naive() == today - Duration::days(1));
    }
    if tag_name.eq_ignore_ascii_case("WEEK") {
        return is_actionable
            && task.get_due().is_some_and(|due| {
                let due_date = due.date_naive();
                let sow = today - Duration::days(today.weekday().num_days_from_monday() as i64);
                let eow = sow + Duration::days(6);
                due_date >= sow && due_date <= eow
            });
    }
    if tag_name.eq_ignore_ascii_case("MONTH") {
        return is_actionable
            && task.get_due().is_some_and(|due| {
                let d = due.date_naive();
                d.year() == today.year() && d.month() == today.month()
            });
    }
    if tag_name.eq_ignore_ascii_case("YEAR") {
        return is_actionable
            && task
                .get_due()
                .is_some_and(|due| due.date_naive().year() == today.year());
    }
    if tag_name.eq_ignore_ascii_case("READY") {
        return task.get_status() == Status::Pending
            && !task.is_blocked()
            && task.get_wait().is_none_or(|wait| wait <= now)
            && task
                .get_value("scheduled")
                .map(|s| {
                    s.parse::<i64>()
                        .ok()
                        .and_then(|secs| DateTime::from_timestamp(secs, 0))
                        .is_some_and(|dt| dt <= now)
                })
                .unwrap_or(true);
    }
    if tag_name.eq_ignore_ascii_case("TAGGED") {
        return task.get_tags().any(|t| t.is_user());
    }
    if tag_name.eq_ignore_ascii_case("ANNOTATED") {
        return task.get_annotations().next().is_some();
    }
    if tag_name.eq_ignore_ascii_case("PROJECT") {
        return task.get_value("project").is_some();
    }
    if tag_name.eq_ignore_ascii_case("PRIORITY") {
        return !task.get_priority().is_empty();
    }
    if tag_name.eq_ignore_ascii_case("SCHEDULED") {
        return task.get_value("scheduled").is_some();
    }
    false
}

/// Evaluate an attribute comparison.
fn eval_attribute(
    task: &Task,
    name: &str,
    modifier: &AttrModifier,
    value: &str,
    parsed_date: Option<DateTime<Utc>>,
    _ctx: &EvalContext,
) -> bool {
    match name {
        "status" => eval_status(task, modifier, value),
        "project" => eval_string_attr(task.get_value("project"), modifier, value),
        "priority" => {
            let p = task.get_priority();
            let task_val = if p.is_empty() { None } else { Some(p) };
            eval_string_attr(task_val, modifier, value)
        }
        "description" => eval_string_attr(Some(task.get_description()), modifier, value),
        "due" => eval_date_attr(task.get_due(), modifier, value, parsed_date),
        "entry" => eval_date_attr(task.get_entry(), modifier, value, parsed_date),
        "modified" => eval_date_attr(task.get_modified(), modifier, value, parsed_date),
        "wait" => eval_date_attr(task.get_wait(), modifier, value, parsed_date),
        "scheduled" => {
            let dt = task
                .get_value("scheduled")
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|secs| DateTime::from_timestamp(secs, 0));
            eval_date_attr(dt, modifier, value, parsed_date)
        }
        "tags" => match modifier {
            AttrModifier::Has => Tag::try_from(value).is_ok_and(|tag| task.has_tag(&tag)),
            AttrModifier::Hasnt => Tag::try_from(value).is_ok_and(|tag| !task.has_tag(&tag)),
            AttrModifier::Any => task.get_tags().any(|t| t.is_user()),
            AttrModifier::None => !task.get_tags().any(|t| t.is_user()),
            _ => false,
        },
        _ => eval_string_attr(task.get_value(name), modifier, value),
    }
}

fn eval_status(task: &Task, modifier: &AttrModifier, value: &str) -> bool {
    let status_str = match task.get_status() {
        Status::Pending => "pending",
        Status::Completed => "completed",
        Status::Deleted => "deleted",
        Status::Recurring => "recurring",
        _ => "unknown",
    };

    match modifier {
        AttrModifier::Equals | AttrModifier::Is => status_str.eq_ignore_ascii_case(value),
        AttrModifier::Isnt => !status_str.eq_ignore_ascii_case(value),
        _ => status_str.eq_ignore_ascii_case(value),
    }
}

/// ASCII case-insensitive prefix check without allocation.
fn starts_with_ignore_case(haystack: &str, needle: &str) -> bool {
    if haystack.len() < needle.len() {
        return false;
    }
    haystack.as_bytes()[..needle.len()]
        .iter()
        .zip(needle.as_bytes())
        .all(|(h, n)| h.eq_ignore_ascii_case(n))
}

/// ASCII case-insensitive suffix check without allocation.
fn ends_with_ignore_case(haystack: &str, needle: &str) -> bool {
    if haystack.len() < needle.len() {
        return false;
    }
    let offset = haystack.len() - needle.len();
    haystack.as_bytes()[offset..]
        .iter()
        .zip(needle.as_bytes())
        .all(|(h, n)| h.eq_ignore_ascii_case(n))
}

/// ASCII case-insensitive contains check without allocation.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    let needle_bytes = needle.as_bytes();
    let first = needle_bytes[0].to_ascii_lowercase();
    for i in 0..=(haystack.len() - needle.len()) {
        if haystack.as_bytes()[i].to_ascii_lowercase() == first
            && haystack.as_bytes()[i..i + needle.len()]
                .iter()
                .zip(needle_bytes)
                .all(|(h, n)| h.eq_ignore_ascii_case(n))
        {
            return true;
        }
    }
    false
}

fn eval_string_attr(task_val: Option<&str>, modifier: &AttrModifier, filter_val: &str) -> bool {
    match modifier {
        AttrModifier::None => return task_val.is_none() || task_val.is_some_and(|v| v.is_empty()),
        AttrModifier::Any => return task_val.is_some_and(|v| !v.is_empty()),
        _ => {}
    }

    let task_str = match task_val {
        Some(s) => s,
        None => {
            return matches!(modifier, AttrModifier::None)
                || (matches!(modifier, AttrModifier::Equals) && filter_val.is_empty());
        }
    };

    match modifier {
        AttrModifier::Equals => {
            if filter_val.is_empty() {
                task_str.is_empty()
            } else {
                starts_with_ignore_case(task_str, filter_val)
            }
        }
        AttrModifier::Is => task_str.eq_ignore_ascii_case(filter_val),
        AttrModifier::Isnt => !task_str.eq_ignore_ascii_case(filter_val),
        AttrModifier::Has => contains_ignore_case(task_str, filter_val),
        AttrModifier::Hasnt => !contains_ignore_case(task_str, filter_val),
        AttrModifier::StartsWith => starts_with_ignore_case(task_str, filter_val),
        AttrModifier::EndsWith => ends_with_ignore_case(task_str, filter_val),
        AttrModifier::Before => task_str < filter_val,
        AttrModifier::After => task_str > filter_val,
        AttrModifier::By => task_str <= filter_val,
        AttrModifier::None => task_str.is_empty(),
        AttrModifier::Any => !task_str.is_empty(),
    }
}

fn eval_date_attr(
    task_date: Option<DateTime<Utc>>,
    modifier: &AttrModifier,
    filter_val: &str,
    pre_parsed: Option<DateTime<Utc>>,
) -> bool {
    match modifier {
        AttrModifier::None => return task_date.is_none(),
        AttrModifier::Any => return task_date.is_some(),
        _ => {}
    }

    let task_dt = match task_date {
        Some(dt) => dt,
        None => return false,
    };

    // Use pre-parsed date from AST if available (avoids per-task parsing)
    let filter_dt = match pre_parsed.or_else(|| parse_date_value(filter_val)) {
        Some(dt) => dt,
        None => return false,
    };

    match modifier {
        AttrModifier::Equals => task_dt.date_naive() == filter_dt.date_naive(),
        AttrModifier::Is => task_dt == filter_dt,
        AttrModifier::Isnt => task_dt != filter_dt,
        AttrModifier::Before => task_dt < filter_dt,
        AttrModifier::After => task_dt > filter_dt,
        AttrModifier::By => task_dt <= filter_dt,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_structure() {
        let expr = parse_filter("status:pending +shopping project:PERSONAL.Home");
        assert!(matches!(expr, FilterExpr::And(_, _)));
    }

    #[test]
    fn test_complex_or() {
        let expr = parse_filter("status:pending (+DUETODAY or +OVERDUE)");
        match expr {
            FilterExpr::And(left, right) => {
                assert!(matches!(*left, FilterExpr::Attribute { .. }));
                assert!(matches!(*right, FilterExpr::Or(_, _)));
            }
            _ => panic!("Expected And(Attribute, Or(...))"),
        }
    }

    #[test]
    fn test_starts_with_ignore_case() {
        assert!(starts_with_ignore_case("PERSONAL.Home", "personal"));
        assert!(starts_with_ignore_case("personal.home", "PERSONAL"));
        assert!(!starts_with_ignore_case("PER", "PERSONAL"));
    }

    #[test]
    fn test_contains_ignore_case() {
        assert!(contains_ignore_case("Hello World", "world"));
        assert!(contains_ignore_case("HELLO", "ell"));
        assert!(!contains_ignore_case("Hello", "xyz"));
        assert!(contains_ignore_case("anything", ""));
    }

    #[test]
    fn test_ends_with_ignore_case() {
        assert!(ends_with_ignore_case("Hello World", "WORLD"));
        assert!(ends_with_ignore_case("test.txt", ".TXT"));
        assert!(!ends_with_ignore_case("short", "longerthan"));
    }

    // --- Helper to create a task with specific properties via InMemoryStorage ---

    use taskchampion::storage::inmemory::InMemoryStorage;
    use taskchampion::{Operations, Replica, Uuid};

    /// Create a pending task with a due date and return (replica, task_uuid).
    /// `due` is optional — pass None for tasks without a due date.
    async fn make_task_with_due(
        replica: &mut Replica<InMemoryStorage>,
        description: &str,
        due: Option<DateTime<Utc>>,
    ) -> Uuid {
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description(description.to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        if let Some(dt) = due {
            task.set_due(Some(dt), &mut ops).unwrap();
        }
        replica.commit_operations(ops).await.unwrap();
        uuid
    }

    /// Build an EvalContext with a deterministic "now" so tests don't depend on wall-clock time.
    fn ctx_at(now: DateTime<Utc>) -> EvalContext {
        EvalContext {
            today: now.date_naive(),
            now,
        }
    }

    /// Deterministic "now" used throughout the virtual-tag and modifier tests.
    fn fixed_now() -> DateTime<Utc> {
        // 2026-03-29 12:00:00 UTC
        NaiveDate::from_ymd_opt(2026, 3, 29)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
    }

    // ========== Virtual-tag tests ==========

    #[tokio::test]
    async fn test_overdue_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let yesterday = now - Duration::days(1);
        let uuid = make_task_with_due(&mut replica, "overdue task", Some(yesterday)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "OVERDUE", &ctx),
            "Task due yesterday should be OVERDUE"
        );
    }

    #[tokio::test]
    async fn test_overdue_future_not_matching() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let tomorrow = now + Duration::days(1);
        let uuid = make_task_with_due(&mut replica, "future task", Some(tomorrow)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            !eval_virtual_tag(&task, "OVERDUE", &ctx),
            "Task due tomorrow should NOT be OVERDUE"
        );
    }

    #[tokio::test]
    async fn test_duetoday_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        // Due at start of today
        let today_midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let uuid = make_task_with_due(&mut replica, "today task", Some(today_midnight)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "DUETODAY", &ctx),
            "Task due today should match +DUETODAY"
        );
    }

    #[tokio::test]
    async fn test_duetoday_yesterday_not_matching() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let yesterday = now - Duration::days(1);
        let uuid = make_task_with_due(&mut replica, "yesterday task", Some(yesterday)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            !eval_virtual_tag(&task, "DUETODAY", &ctx),
            "Task due yesterday should NOT match +DUETODAY"
        );
    }

    #[tokio::test]
    async fn test_due_within_week() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let in_3_days = now + Duration::days(3);
        let uuid = make_task_with_due(&mut replica, "soon task", Some(in_3_days)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "DUE", &ctx),
            "Task due in 3 days should match +DUE (within 7 day window)"
        );
    }

    #[tokio::test]
    async fn test_due_overdue_also_matches() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let yesterday = now - Duration::days(1);
        let uuid = make_task_with_due(&mut replica, "overdue task", Some(yesterday)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "DUE", &ctx),
            "Overdue task (due yesterday, -1 <= 7) should also match +DUE"
        );
    }

    #[tokio::test]
    async fn test_due_beyond_week_not_matching() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let in_10_days = now + Duration::days(10);
        let uuid = make_task_with_due(&mut replica, "far task", Some(in_10_days)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            !eval_virtual_tag(&task, "DUE", &ctx),
            "Task due in 10 days should NOT match +DUE"
        );
    }

    // ========== Modifier tests ==========

    #[tokio::test]
    async fn test_string_modifier_has() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let uuid = make_task_with_due(&mut replica, "buy foo bar", None).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(fixed_now());

        assert!(
            eval_attribute(&task, "description", &AttrModifier::Has, "foo", None, &ctx),
            "description.has:foo should match 'buy foo bar'"
        );
        assert!(
            !eval_attribute(&task, "description", &AttrModifier::Has, "baz", None, &ctx),
            "description.has:baz should NOT match 'buy foo bar'"
        );
    }

    #[tokio::test]
    async fn test_string_modifier_equals() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("work task".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_value("project", Some("WORK".to_string()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(fixed_now());

        // project:WORK uses Equals modifier which is starts-with
        assert!(
            eval_attribute(&task, "project", &AttrModifier::Equals, "WORK", None, &ctx),
            "project:WORK should match project 'WORK' (starts-with)"
        );
    }

    #[tokio::test]
    async fn test_date_modifier_before() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        // Task due today (start of day)
        let today_midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let uuid = make_task_with_due(&mut replica, "today task", Some(today_midnight)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        // due.before:tomorrow — tomorrow is 2026-03-30T00:00:00Z
        let tomorrow_dt = (now.date_naive() + Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        assert!(
            eval_attribute(
                &task,
                "due",
                &AttrModifier::Before,
                "tomorrow",
                Some(tomorrow_dt),
                &ctx
            ),
            "due.before:tomorrow should match task due today"
        );
    }

    #[tokio::test]
    async fn test_date_modifier_after() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let today_midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let uuid = make_task_with_due(&mut replica, "today task", Some(today_midnight)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        // due.after:yesterday — yesterday is 2026-03-28T00:00:00Z
        let yesterday_dt = (now.date_naive() - Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        assert!(
            eval_attribute(
                &task,
                "due",
                &AttrModifier::After,
                "yesterday",
                Some(yesterday_dt),
                &ctx
            ),
            "due.after:yesterday should match task due today"
        );
    }

    #[tokio::test]
    async fn test_tags_has_modifier() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("buy groceries".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        let tag = Tag::try_from("shopping").unwrap();
        task.add_tag(&tag, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(fixed_now());

        assert!(
            eval_attribute(&task, "tags", &AttrModifier::Has, "shopping", None, &ctx),
            "tags.has:shopping should match task with +shopping tag"
        );
        assert!(
            !eval_attribute(&task, "tags", &AttrModifier::Has, "work", None, &ctx),
            "tags.has:work should NOT match task with only +shopping tag"
        );
    }

    // ========== Additional virtual tag tests (codex review) ==========

    #[tokio::test]
    async fn test_completed_task_not_overdue() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let past = now - Duration::days(5);

        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("done task".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Completed, &mut ops).unwrap();
        task.set_due(Some(past), &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            !eval_virtual_tag(&task, "OVERDUE", &ctx),
            "Completed task with past due should NOT match +OVERDUE"
        );
    }

    #[tokio::test]
    async fn test_deleted_task_not_duetoday() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let today_midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("deleted task".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Deleted, &mut ops).unwrap();
        task.set_due(Some(today_midnight), &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            !eval_virtual_tag(&task, "DUETODAY", &ctx),
            "Deleted task should NOT match +DUETODAY"
        );
    }

    #[tokio::test]
    async fn test_due_long_overdue() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        let long_ago = now - Duration::days(30);
        let uuid = make_task_with_due(&mut replica, "very late task", Some(long_ago)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "DUE", &ctx),
            "Task 30 days overdue (-30 <= 7) should still match +DUE"
        );
    }

    #[tokio::test]
    async fn test_tagged_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);

        // Task WITH tags
        let mut ops = Operations::new();
        let uuid_tagged = Uuid::new_v4();
        let mut task = replica.create_task(uuid_tagged, &mut ops).await.unwrap();
        task.set_description("tagged task".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        let tag = Tag::try_from("work").unwrap();
        task.add_tag(&tag, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();

        // Task WITHOUT tags
        let uuid_untagged = make_task_with_due(&mut replica, "untagged task", None).await;

        let ctx = ctx_at(fixed_now());

        let tagged_task = replica.get_task(uuid_tagged).await.unwrap().unwrap();
        assert!(
            eval_virtual_tag(&tagged_task, "TAGGED", &ctx),
            "Task with user tags should match +TAGGED"
        );

        let untagged_task = replica.get_task(uuid_untagged).await.unwrap().unwrap();
        assert!(
            !eval_virtual_tag(&untagged_task, "TAGGED", &ctx),
            "Task without tags should NOT match +TAGGED"
        );
    }

    #[tokio::test]
    async fn test_date_modifier_equals_same_day() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        // Task due at 15:30 today (different time from midnight)
        let due_time = now.date_naive().and_hms_opt(15, 30, 0).unwrap().and_utc();
        let uuid = make_task_with_due(&mut replica, "afternoon task", Some(due_time)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        // due.equals:today — "today" resolves to start of day (midnight)
        let today_midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        assert!(
            eval_attribute(
                &task,
                "due",
                &AttrModifier::Equals,
                "today",
                Some(today_midnight),
                &ctx
            ),
            "due.equals:today should match task due same day at different time"
        );
    }

    // ========== Negative / boundary modifier tests (codex review) ==========

    #[tokio::test]
    async fn test_string_modifier_before_strict() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);

        // Create task with description "aaa"
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("aaa".to_string(), &mut ops).unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(fixed_now());

        assert!(
            eval_attribute(
                &task,
                "description",
                &AttrModifier::Before,
                "aab",
                None,
                &ctx
            ),
            "\"aaa\".before:\"aab\" should match (strict less-than)"
        );

        // Create task with description "aab"
        let mut ops = Operations::new();
        let uuid2 = Uuid::new_v4();
        let mut task2 = replica.create_task(uuid2, &mut ops).await.unwrap();
        task2.set_description("aab".to_string(), &mut ops).unwrap();
        task2.set_status(Status::Pending, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task2 = replica.get_task(uuid2).await.unwrap().unwrap();
        assert!(
            !eval_attribute(
                &task2,
                "description",
                &AttrModifier::Before,
                "aab",
                None,
                &ctx
            ),
            "\"aab\".before:\"aab\" should NOT match (strict less-than, not <=)"
        );
    }

    #[tokio::test]
    async fn test_tags_none_modifier() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);

        // Task with no user tags
        let uuid = make_task_with_due(&mut replica, "plain task", None).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(fixed_now());

        assert!(
            eval_attribute(&task, "tags", &AttrModifier::None, "", None, &ctx),
            "tags.none: should match task with no user tags"
        );
    }

    #[tokio::test]
    async fn test_tags_any_modifier() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let ctx = ctx_at(fixed_now());

        // Task WITH tags
        let mut ops = Operations::new();
        let uuid_with = Uuid::new_v4();
        let mut task = replica.create_task(uuid_with, &mut ops).await.unwrap();
        task.set_description("tagged".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        let tag = Tag::try_from("urgent").unwrap();
        task.add_tag(&tag, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task_with = replica.get_task(uuid_with).await.unwrap().unwrap();
        assert!(
            eval_attribute(&task_with, "tags", &AttrModifier::Any, "", None, &ctx),
            "tags.any: should match task with user tags"
        );

        // Task WITHOUT tags
        let uuid_without = make_task_with_due(&mut replica, "no tags", None).await;
        let task_without = replica.get_task(uuid_without).await.unwrap().unwrap();
        assert!(
            !eval_attribute(&task_without, "tags", &AttrModifier::Any, "", None, &ctx),
            "tags.any: should NOT match task without user tags"
        );
    }

    #[tokio::test]
    async fn test_date_modifier_equals_vs_is() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();
        // Task due at 15:30 today
        let due_time = now.date_naive().and_hms_opt(15, 30, 0).unwrap().and_utc();
        let uuid = make_task_with_due(&mut replica, "afternoon task", Some(due_time)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        let today_midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        // due.equals:today — compares date part only, should match
        assert!(
            eval_attribute(
                &task,
                "due",
                &AttrModifier::Equals,
                "today",
                Some(today_midnight),
                &ctx
            ),
            "due.equals:today should match same-day (date comparison)"
        );

        // due.is:today — compares exact timestamp, should NOT match (midnight != 15:30)
        assert!(
            !eval_attribute(
                &task,
                "due",
                &AttrModifier::Is,
                "today",
                Some(today_midnight),
                &ctx
            ),
            "due.is:today should NOT match when times differ (exact timestamp comparison)"
        );
    }

    // ========== WEEK / MONTH / YEAR / READY / SCHEDULED virtual tag tests ==========

    #[tokio::test]
    async fn test_week_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now(); // 2026-03-29 (Sunday)
                               // Due on Wednesday of the same week (March 25)
        let wed = now - Duration::days(4);
        let uuid = make_task_with_due(&mut replica, "midweek task", Some(wed)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "WEEK", &ctx),
            "Task due this week (Wed) should match +WEEK"
        );

        // Task due next week should NOT match
        let next_week = now + Duration::days(2); // Tuesday next week
        let uuid2 = make_task_with_due(&mut replica, "next week task", Some(next_week)).await;
        let task2 = replica.get_task(uuid2).await.unwrap().unwrap();
        assert!(
            !eval_virtual_tag(&task2, "WEEK", &ctx),
            "Task due next week should NOT match +WEEK"
        );
    }

    #[tokio::test]
    async fn test_week_boundary_monday() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now(); // 2026-03-29 (Sunday)
                               // Monday of the same week = March 23
        let monday = now - Duration::days(6);
        let uuid = make_task_with_due(&mut replica, "monday task", Some(monday)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "WEEK", &ctx),
            "Task due on Monday of current week should match +WEEK"
        );
    }

    #[tokio::test]
    async fn test_month_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now(); // 2026-03-29

        // Due March 1 — same month
        let march_1 = NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let uuid = make_task_with_due(&mut replica, "this month", Some(march_1)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "MONTH", &ctx),
            "Task due this month should match +MONTH"
        );

        // Due April 1 — next month
        let april_1 = NaiveDate::from_ymd_opt(2026, 4, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let uuid2 = make_task_with_due(&mut replica, "next month", Some(april_1)).await;
        let task2 = replica.get_task(uuid2).await.unwrap().unwrap();
        assert!(
            !eval_virtual_tag(&task2, "MONTH", &ctx),
            "Task due next month should NOT match +MONTH"
        );
    }

    #[tokio::test]
    async fn test_year_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now(); // 2026-03-29

        // Due Dec 31, 2026 — same year
        let dec_31 = NaiveDate::from_ymd_opt(2026, 12, 31)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let uuid = make_task_with_due(&mut replica, "this year", Some(dec_31)).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            eval_virtual_tag(&task, "YEAR", &ctx),
            "Task due this year should match +YEAR"
        );

        // Due Jan 1, 2027 — next year
        let jan_1_next = NaiveDate::from_ymd_opt(2027, 1, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let uuid2 = make_task_with_due(&mut replica, "next year", Some(jan_1_next)).await;
        let task2 = replica.get_task(uuid2).await.unwrap().unwrap();
        assert!(
            !eval_virtual_tag(&task2, "YEAR", &ctx),
            "Task due next year should NOT match +YEAR"
        );
    }

    #[tokio::test]
    async fn test_ready_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        // Pending task with no wait/scheduled → READY
        let uuid = make_task_with_due(&mut replica, "ready task", None).await;
        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(fixed_now());

        assert!(
            eval_virtual_tag(&task, "READY", &ctx),
            "Pending task with no wait/scheduled should match +READY"
        );
    }

    #[tokio::test]
    async fn test_ready_waiting_not_matching() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let now = fixed_now();

        // Pending task with future wait date → NOT READY
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("waiting task".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_wait(Some(now + Duration::days(5)), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(now);

        assert!(
            !eval_virtual_tag(&task, "READY", &ctx),
            "Task with future wait date should NOT match +READY"
        );
    }

    #[tokio::test]
    async fn test_scheduled_virtual_tag() {
        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);

        // Task with scheduled property
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("scheduled task".to_string(), &mut ops)
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        // scheduled is stored as epoch seconds
        task.set_value("scheduled", Some("1711814400".to_string()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        let ctx = ctx_at(fixed_now());

        assert!(
            eval_virtual_tag(&task, "SCHEDULED", &ctx),
            "Task with scheduled property should match +SCHEDULED"
        );

        // Task without scheduled property
        let uuid2 = make_task_with_due(&mut replica, "plain task", None).await;
        let task2 = replica.get_task(uuid2).await.unwrap().unwrap();
        assert!(
            !eval_virtual_tag(&task2, "SCHEDULED", &ctx),
            "Task without scheduled property should NOT match +SCHEDULED"
        );
    }
}
