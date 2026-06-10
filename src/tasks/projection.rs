//! TaskItem wire-shape projection.
//!
//! Converts a TaskChampion `Task` into our public REST `TaskItem`. Lives
//! in `tasks/` (not in storage infrastructure) because every API contract
//! change — annotations, depends, urgency factors, UDA pass-through —
//! lands here and should be reviewed alongside other task-shape changes.
//! Per ADR-0002 review 2026-05-04 § P5 / issue server#126.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use taskchampion::Status;
use uuid::Uuid;

use crate::tasks::models::TaskItem;

/// Convert a TaskChampion Task to our API TaskItem model.
///
/// When `pending_uuids` is `Some`, dependency UUIDs are filtered to pending-only
/// (authoritative). When `None` (mutation/scheduler paths), `depends` is empty.
/// `blocked: bool` is always correct regardless.
///
/// When `task_keys` is `Some`, the per-task `key` is looked up by canonical
/// UUID string (the value of `task.get_uuid().to_string()`). When `None` or
/// the UUID is absent from the map, `key` is left as `None` on the wire.
/// Source of truth is the `task_key_allocations` table, NOT TC's
/// `cmdock_key` UDA — `task-write-contract.md` § Task Keys (server#130 C7).
pub fn task_to_item(
    task: &taskchampion::Task,
    pending_uuids: Option<&HashSet<Uuid>>,
    task_keys: Option<&HashMap<String, String>>,
) -> TaskItem {
    let now = Utc::now();
    let project = task.get_value("project").map(|s| s.to_string());
    let tags: Vec<String> = task
        .get_tags()
        .filter(|t| t.is_user())
        .map(|t| t.to_string())
        .collect();
    let priority_str = task.get_priority();
    let priority = if priority_str.is_empty() {
        None
    } else {
        Some(priority_str.to_string())
    };
    let due = task.get_due();
    let blocked = task.is_blocked();
    let waiting = task.get_wait().is_some_and(|wait| wait > now);

    // Resolve pending dependency UUIDs (sorted for deterministic output).
    // Only populated when the caller provides the pending set (list/snapshot paths).
    let depends = {
        let mut dep_uuids: Vec<Uuid> = match pending_uuids {
            Some(set) => task
                .get_dependencies()
                .filter(|u| set.contains(u))
                .collect(),
            None => Vec::new(),
        };
        dep_uuids.sort();
        dep_uuids.iter().map(|u| u.to_string()).collect()
    };

    let status = match task.get_status() {
        Status::Pending => "pending",
        Status::Completed => "completed",
        Status::Deleted => "deleted",
        Status::Recurring => "recurring",
        _ => "pending",
    };

    // Annotations: sort by entry time (oldest first) so chronological renderers
    // don't depend on TC's iteration order, which is not contractually stable.
    let mut annotations: Vec<crate::tasks::models::TaskAnnotation> = task
        .get_annotations()
        .map(|a| crate::tasks::models::TaskAnnotation {
            entry: format_tw_date(a.entry),
            description: a.description,
        })
        .collect();
    annotations.sort_by(|a, b| a.entry.cmp(&b.entry));

    let uuid_str = task.get_uuid().to_string();
    let key = task_keys
        .and_then(|m| m.get(&uuid_str))
        .map(|s| s.to_string());
    let task_scope = key
        .as_deref()
        .and_then(|key| key.split_once('-').map(|(prefix, _)| prefix.to_string()));

    TaskItem {
        uuid: uuid_str,
        description: task.get_description().to_string(),
        project,
        tags,
        priority,
        due: due.map(format_tw_date),
        urgency: crate::tasks::urgency::urgency_for_task(task, now),
        depends,
        blocked,
        waiting,
        status: status.to_string(),
        key,
        cmdock_task_scope: task_scope,
        annotations,
        extra: crate::tasks::extract_udas(task),
    }
}

/// Format a chrono DateTime to Taskwarrior date format: YYYYMMDDTHHmmssZ
fn format_tw_date(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replica::parse_tw_date;

    #[test]
    fn test_format_tw_date_roundtrip() {
        let dt = chrono::Utc::now();
        let formatted = format_tw_date(dt);
        let parsed = parse_tw_date(&formatted);
        assert!(parsed.is_some(), "roundtrip should succeed");
        // Precision is seconds, so truncate the original
        let expected = dt.format("%Y%m%dT%H%M%SZ").to_string();
        assert_eq!(formatted, expected);
    }

    // --- Annotation surface in REST TaskItem -------------------------------
    //
    // Annotations live in the TC replica but were historically not serialized
    // by `task_to_item`. The iOS markdown renderer landed assuming standard
    // Taskwarrior JSON shape (annotations: [TaskAnnotation]). These tests
    // pin the contract so an iOS regression cannot land silently.

    #[tokio::test]
    async fn test_task_to_item_includes_annotations_when_present() {
        use taskchampion::storage::inmemory::InMemoryStorage;
        use taskchampion::{Annotation, Operations, Replica, Uuid};

        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("review board pack".into(), &mut ops)
            .unwrap();
        task.set_status(taskchampion::Status::Pending, &mut ops)
            .unwrap();

        // Two annotations, intentionally added out of chronological order to
        // verify task_to_item sorts ascending by entry.
        let later = "2026-05-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let earlier = "2026-04-29T08:00:00Z".parse::<DateTime<Utc>>().unwrap();
        task.add_annotation(
            Annotation {
                entry: later,
                description: "**Decision:** ship v0.1.1 next sprint".into(),
            },
            &mut ops,
        )
        .unwrap();
        task.add_annotation(
            Annotation {
                entry: earlier,
                description: "Initial scoping note — see [issue 92](#92)".into(),
            },
            &mut ops,
        )
        .unwrap();

        replica.commit_operations(ops).await.unwrap();
        let task = replica.get_task(uuid).await.unwrap().unwrap();

        let item = task_to_item(&task, None, None);
        assert_eq!(item.annotations.len(), 2);
        // Sorted oldest first.
        assert_eq!(item.annotations[0].entry, "20260429T080000Z");
        assert_eq!(item.annotations[1].entry, "20260501T120000Z");
        assert_eq!(
            item.annotations[0].description,
            "Initial scoping note — see [issue 92](#92)"
        );
        assert_eq!(
            item.annotations[1].description,
            "**Decision:** ship v0.1.1 next sprint"
        );

        // JSON shape: emits the annotations array as top-level field with
        // entry/description objects (matches the iOS decoder's expectation).
        let json = serde_json::to_value(&item).unwrap();
        let arr = json
            .get("annotations")
            .expect("annotations key present")
            .as_array()
            .expect("annotations is array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["entry"], "20260429T080000Z");
        assert_eq!(
            arr[0]["description"],
            "Initial scoping note — see [issue 92](#92)"
        );
    }

    #[tokio::test]
    async fn test_task_to_item_omits_annotations_key_when_empty() {
        use taskchampion::storage::inmemory::InMemoryStorage;
        use taskchampion::{Operations, Replica, Uuid};

        let storage = InMemoryStorage::new();
        let mut replica = Replica::new(storage);
        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_description("vanilla task".into(), &mut ops)
            .unwrap();
        task.set_status(taskchampion::Status::Pending, &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        let task = replica.get_task(uuid).await.unwrap().unwrap();

        let item = task_to_item(&task, None, None);
        assert!(item.annotations.is_empty());

        // skip_serializing_if = "Vec::is_empty" should keep the field out of
        // the JSON entirely so iOS `decodeIfPresent` resolves to None.
        let json = serde_json::to_value(&item).unwrap();
        assert!(
            json.get("annotations").is_none(),
            "annotations key must be absent when empty (got: {json})"
        );
    }
}
