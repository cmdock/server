//! Default (built-in) view definitions.
//!
//! These views are seeded on user creation and reconciled on first auth.
//! Users can customise or hide them — the server tracks `user_modified`
//! and `hidden` flags to avoid clobbering edits or re-adding deleted views.
//!
//! Bump `VIEWSET_VERSION` when adding or changing built-in views.
//! The reconcile logic will create new views and optionally update
//! unmodified views to the latest version.

use crate::store::models::ViewRecord;

/// Current version of the default viewset.
/// Bump this when adding new views or changing existing built-in filters.
pub const VIEWSET_VERSION: i32 = 5;

/// Return the default built-in views for a new user.
pub fn default_views() -> Vec<ViewRecord> {
    vec![
        // Cross-cutting views: context_filtered=true, no context_id.
        // Client shows segmented control; user selects scope.
        ViewRecord {
            id: "duesoon".to_string(),
            label: "Due Soon".to_string(),
            icon: "clock".to_string(),
            filter: "status:pending -BLOCKED -WAITING due.before:7d".to_string(),
            group_by: None,
            context_filtered: true,
            display_mode: "list".to_string(),
            sort_order: 10,
            origin: "builtin".to_string(),
            user_modified: false,
            hidden: false,
            template_version: VIEWSET_VERSION,
            context_id: None,
        },
        ViewRecord {
            id: "action".to_string(),
            label: "Action".to_string(),
            icon: "bolt".to_string(),
            filter: "status:pending -BLOCKED -WAITING priority:H".to_string(),
            group_by: None,
            context_filtered: true,
            display_mode: "list".to_string(),
            sort_order: 20,
            origin: "builtin".to_string(),
            user_modified: false,
            hidden: false,
            template_version: VIEWSET_VERSION,
            context_id: None,
        },
        // Project-scoped named views: context_filtered=true, context_id set.
        // Client auto-applies the bound context's projectPrefixes.
        ViewRecord {
            id: "personal".to_string(),
            label: "Personal".to_string(),
            icon: "person".to_string(),
            filter: "status:pending".to_string(),
            group_by: Some("project".to_string()),
            context_filtered: true,
            display_mode: "grouped".to_string(),
            sort_order: 30,
            origin: "builtin".to_string(),
            user_modified: false,
            hidden: false,
            template_version: VIEWSET_VERSION,
            context_id: Some("personal".to_string()),
        },
        ViewRecord {
            id: "work".to_string(),
            label: "Work".to_string(),
            icon: "briefcase".to_string(),
            filter: "status:pending".to_string(),
            group_by: Some("project".to_string()),
            context_filtered: true,
            display_mode: "grouped".to_string(),
            sort_order: 40,
            origin: "builtin".to_string(),
            user_modified: false,
            hidden: false,
            template_version: VIEWSET_VERSION,
            context_id: Some("work".to_string()),
        },
        ViewRecord {
            id: "health".to_string(),
            label: "Health".to_string(),
            icon: "heart".to_string(),
            filter: "status:pending".to_string(),
            group_by: None,
            context_filtered: true,
            display_mode: "list".to_string(),
            sort_order: 50,
            origin: "builtin".to_string(),
            user_modified: false,
            hidden: false,
            template_version: VIEWSET_VERSION,
            context_id: Some("health".to_string()),
        },
        // Tag-scoped named view: context_filtered=false, no context_id.
        // Server filter is self-contained.
        ViewRecord {
            id: "shopping".to_string(),
            label: "Shopping".to_string(),
            icon: "cart".to_string(),
            filter: "status:pending +shopping".to_string(),
            group_by: None,
            context_filtered: false,
            display_mode: "list".to_string(),
            sort_order: 60,
            origin: "builtin".to_string(),
            user_modified: false,
            hidden: false,
            template_version: VIEWSET_VERSION,
            context_id: None,
        },
    ]
}

pub fn builtin_view(id: &str) -> Option<ViewRecord> {
    default_views().into_iter().find(|view| view.id == id)
}

fn is_actionable_builtin(id: &str) -> bool {
    matches!(id, "duesoon" | "action")
}

fn ensure_actionable_exclusions(filter: &str) -> String {
    let has_blocked = filter
        .split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("-BLOCKED"));
    let has_waiting = filter
        .split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("-WAITING"));

    let mut normalized = filter.trim().to_string();
    if !has_blocked {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str("-BLOCKED");
    }
    if !has_waiting {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str("-WAITING");
    }
    normalized
}

/// Known v4 project-scoped filters keyed by view ID.  Used during the v4→v5
/// reconciliation to broaden stale hardcoded filters even when
/// `user_modified=true`.  A user who only renamed the label still gets the
/// broadened filter; a user who wrote a genuinely custom filter (e.g.
/// `status:pending project:MYWORK`) keeps theirs.
///
/// Keyed by ID so that a user-modified `work` view with filter
/// `status:pending project:PERSONAL` is not accidentally matched.
fn v4_project_scoped_filter_for(id: &str) -> Option<&'static str> {
    match id {
        "personal" => Some("status:pending project:PERSONAL"),
        "work" => Some("status:pending project:WORK"),
        "health" => Some("status:pending project:HEALTH"),
        _ => None,
    }
}

fn merge_builtin_view(existing: &ViewRecord, default: &ViewRecord) -> ViewRecord {
    if !existing.user_modified {
        return default.clone();
    }

    let mut merged = existing.clone();
    merged.context_filtered = default.context_filtered;
    merged.context_id = default.context_id.clone();
    merged.template_version = VIEWSET_VERSION;
    if is_actionable_builtin(&merged.id) {
        merged.filter = ensure_actionable_exclusions(&merged.filter);
    }
    // Broaden stale v4 hardcoded project filters to the v5 wide filter.
    // Only replaces the exact known v4 default for this specific view ID —
    // genuinely custom filters like "status:pending project:MYWORK" are preserved.
    if let Some(v4_filter) = v4_project_scoped_filter_for(&merged.id) {
        if merged.filter == v4_filter {
            merged.filter = default.filter.clone();
        }
    }
    merged
}

/// Reconcile built-in views for a user.
///
/// Called on user creation (seed) and lazily on first auth (backfill).
///
/// Rules:
/// - **Missing builtin**: create it (unless hidden tombstone exists)
/// - **Unmodified builtin with older version**: update to latest template
/// - **User-modified builtin**: preserve user-editable fields but still merge
///   server-owned builtin metadata forward
/// - **Hidden (tombstoned) builtin**: leave alone (user explicitly deleted it)
pub async fn reconcile_default_views(
    store: &dyn crate::store::ConfigStore,
    user_id: &str,
) -> anyhow::Result<()> {
    let existing = store.list_views_all(user_id).await?;
    let defaults = default_views();

    for default in &defaults {
        let existing_view = existing.iter().find(|v| v.id == default.id);

        match existing_view {
            None => {
                // Missing — create it
                store.upsert_view(user_id, default).await?;
            }
            Some(v) if v.origin != "builtin" => {
                // User-created view with same ID as a builtin — never overwrite.
                // The user owns this ID; the builtin is effectively shadowed.
                continue;
            }
            Some(v) if v.hidden => {
                // User deleted this builtin — respect the tombstone
                continue;
            }
            Some(v)
                if v.template_version < VIEWSET_VERSION
                    || v.context_filtered != default.context_filtered
                    || v.context_id != default.context_id
                    || (is_actionable_builtin(&v.id)
                        && ensure_actionable_exclusions(&v.filter) != v.filter) =>
            {
                let merged = merge_builtin_view(v, default);
                store.upsert_view(user_id, &merged).await?;
            }
            Some(_) => {
                // Already at current version, not modified — nothing to do
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_views_count() {
        let views = default_views();
        assert_eq!(views.len(), 6, "should have 6 default views");
    }

    #[test]
    fn test_default_views_all_builtin() {
        for v in default_views() {
            assert_eq!(v.origin, "builtin");
            assert!(!v.user_modified);
            assert!(!v.hidden);
            assert_eq!(v.template_version, VIEWSET_VERSION);
        }
    }

    #[test]
    fn test_default_views_unique_ids() {
        let views = default_views();
        let mut ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), views.len(), "all view IDs should be unique");
    }

    #[test]
    fn test_default_views_sorted() {
        let views = default_views();
        let orders: Vec<i32> = views.iter().map(|v| v.sort_order).collect();
        let mut sorted = orders.clone();
        sorted.sort();
        assert_eq!(orders, sorted, "views should be in sort_order");
    }

    #[test]
    fn test_cross_cutting_defaults_are_context_filtered_without_context_id() {
        let views = default_views();
        let duesoon = views.iter().find(|v| v.id == "duesoon").unwrap();
        let action = views.iter().find(|v| v.id == "action").unwrap();

        assert!(duesoon.context_filtered);
        assert!(
            duesoon.context_id.is_none(),
            "cross-cutting views should not have context_id"
        );
        assert!(action.context_filtered);
        assert!(action.context_id.is_none());
    }

    #[test]
    fn test_project_scoped_named_views_have_context_id() {
        let views = default_views();
        for (id, expected_context_id) in [
            ("personal", "personal"),
            ("work", "work"),
            ("health", "health"),
        ] {
            let v = views.iter().find(|v| v.id == id).unwrap();
            assert!(v.context_filtered, "{id} should be context_filtered");
            assert_eq!(
                v.context_id.as_deref(),
                Some(expected_context_id),
                "{id} should have context_id={expected_context_id}"
            );
            assert_eq!(v.filter, "status:pending", "{id} filter should be broad");
        }
    }

    #[test]
    fn test_tag_scoped_view_is_not_context_filtered() {
        let views = default_views();
        let shopping = views.iter().find(|v| v.id == "shopping").unwrap();
        assert!(!shopping.context_filtered);
        assert!(shopping.context_id.is_none());
        assert!(shopping.filter.contains("+shopping"));
    }

    #[test]
    fn test_actionable_exclusions_are_appended_when_missing() {
        assert_eq!(
            ensure_actionable_exclusions("status:pending due.before:3d"),
            "status:pending due.before:3d -BLOCKED -WAITING"
        );
    }

    #[test]
    fn test_actionable_exclusions_are_not_duplicated() {
        assert_eq!(
            ensure_actionable_exclusions("status:pending -BLOCKED due.before:3d -WAITING"),
            "status:pending -BLOCKED due.before:3d -WAITING"
        );
    }
}
