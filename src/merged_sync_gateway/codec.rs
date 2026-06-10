//! TaskChampion history-segment codec boundary for `MergedSyncGateway`.
//!
//! TaskChampion serializes sync operations into a JSON history segment shaped
//! like `{ "operations": [ ... ] }`. The upstream operation enum is private,
//! so this module owns cmdock's de-facto mirror until/unless a public upstream
//! codec exists. No other module should depend on the raw JSON shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Decoded TaskChampion history segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireVersion {
    pub operations: Vec<WireOp>,
}

impl WireVersion {
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

/// De-facto mirror of TaskChampion's private sync operation enum.
///
/// Keep this enum contained to the codec/planner boundary. If TaskChampion adds
/// a new variant, strict deserialization fails here so the gateway can reject or
/// quarantine loudly instead of silently dropping an operation it may have
/// needed for authorization/routing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum WireOp {
    Create {
        uuid: Uuid,
    },
    Delete {
        uuid: Uuid,
    },
    Update {
        uuid: Uuid,
        property: String,
        value: Option<String>,
        timestamp: DateTime<Utc>,
    },
}

impl WireOp {
    pub fn uuid(&self) -> Uuid {
        match self {
            Self::Create { uuid } | Self::Delete { uuid } | Self::Update { uuid, .. } => *uuid,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("decode TaskChampion history segment: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("encode TaskChampion history segment: {0}")]
    Encode(#[source] serde_json::Error),
}

pub fn decode_history_segment(bytes: &[u8]) -> Result<WireVersion, CodecError> {
    serde_json::from_slice(bytes).map_err(CodecError::Decode)
}

#[allow(dead_code)]
pub fn encode_history_segment(version: &WireVersion) -> Result<Vec<u8>, CodecError> {
    serde_json::to_vec(version).map_err(CodecError::Encode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use taskchampion::server::{AddVersionResult, GetVersionResult, SnapshotUrgency, VersionId};
    use taskchampion::storage::inmemory::InMemoryStorage;
    use taskchampion::{Annotation, Operations, Replica, Status, Tag};

    const TASK_CREATE: Uuid = uuid::uuid!("11111111-1111-4111-8111-111111111111");
    const TASK_DELETE: Uuid = uuid::uuid!("22222222-2222-4222-8222-222222222222");
    const TASK_UPDATE: Uuid = uuid::uuid!("33333333-3333-4333-8333-333333333333");
    const TASK_TAGS: Uuid = uuid::uuid!("44444444-4444-4444-8444-444444444444");
    const TASK_DEPENDS: Uuid = uuid::uuid!("55555555-5555-4555-8555-555555555555");
    const TASK_DEPENDENCY: Uuid = uuid::uuid!("55555555-5555-4555-8555-555555555556");
    const TASK_ANNOTATIONS: Uuid = uuid::uuid!("66666666-6666-4666-8666-666666666666");
    const TASK_UDAS: Uuid = uuid::uuid!("77777777-7777-4777-8777-777777777777");
    const TASK_MULTI_SAME: Uuid = uuid::uuid!("88888888-8888-4888-8888-888888888888");
    const TASK_MULTI_A: Uuid = uuid::uuid!("99999999-9999-4999-8999-999999999990");
    const TASK_MULTI_B: Uuid = uuid::uuid!("99999999-9999-4999-8999-999999999991");

    #[test]
    fn decodes_external_tagged_taskchampion_ops() {
        let uuid = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let json = format!(
            r#"{{"operations":[
                {{"Create":{{"uuid":"{uuid}"}}}},
                {{"Update":{{"uuid":"{uuid}","property":"description","value":"hello","timestamp":"2026-05-09T01:02:03Z"}}}},
                {{"Update":{{"uuid":"{uuid}","property":"project","value":null,"timestamp":"2026-05-09T01:02:04Z"}}}},
                {{"Delete":{{"uuid":"{uuid}"}}}}
            ]}}"#
        );

        let decoded = decode_history_segment(json.as_bytes()).unwrap();
        assert_eq!(decoded.operations.len(), 4);
        assert!(matches!(decoded.operations[0], WireOp::Create { uuid: got } if got == uuid));
        assert!(matches!(
            &decoded.operations[1],
            WireOp::Update { uuid: got, property, value: Some(value), .. }
                if *got == uuid && property == "description" && value == "hello"
        ));
        assert!(matches!(
            &decoded.operations[2],
            WireOp::Update { uuid: got, property, value: None, .. }
                if *got == uuid && property == "project"
        ));
        assert!(matches!(decoded.operations[3], WireOp::Delete { uuid: got } if got == uuid));
    }

    #[test]
    fn strict_decode_rejects_unknown_op_variant() {
        let fixture = load_fixture("invalid/unknown_variant.json");
        let err = decode_history_segment(&fixture).unwrap_err();
        assert!(err
            .to_string()
            .contains("decode TaskChampion history segment"));
    }

    #[test]
    fn strict_decode_rejects_extra_update_fields() {
        let fixture = load_fixture("invalid/extra_update_field.json");
        assert!(decode_history_segment(&fixture).is_err());
    }

    #[test]
    fn wire_op_uuid_returns_common_task_uuid() {
        let uuid = Uuid::new_v4();
        assert_eq!(WireOp::Create { uuid }.uuid(), uuid);
        assert_eq!(WireOp::Delete { uuid }.uuid(), uuid);
        assert_eq!(
            WireOp::Update {
                uuid,
                property: "description".to_string(),
                value: Some("hello".to_string()),
                timestamp: "2026-05-09T01:02:03Z".parse().unwrap(),
            }
            .uuid(),
            uuid
        );
    }

    #[test]
    fn fixture_create_decodes() {
        let decoded = decode_fixture("generated/create.json");
        assert_has_create(&decoded, TASK_CREATE);
        assert_has_update(&decoded, TASK_CREATE, "status", Some("pending"));
        assert_has_update(&decoded, TASK_CREATE, "description", Some("fixture create"));
    }

    #[test]
    fn fixture_delete_decodes() {
        let decoded = decode_fixture("generated/delete.json");
        assert_has_update(&decoded, TASK_DELETE, "status", Some("deleted"));
    }

    #[test]
    fn fixture_update_set_decodes() {
        let decoded = decode_fixture("generated/update_set.json");
        assert_has_update(&decoded, TASK_UPDATE, "project", Some("home"));
    }

    #[test]
    fn fixture_update_clear_decodes() {
        let decoded = decode_fixture("generated/update_clear.json");
        assert_has_update(&decoded, TASK_UPDATE, "project", None);
    }

    #[test]
    fn fixture_tags_decodes() {
        let decoded = decode_fixture("generated/tags.json");
        assert_has_update(&decoded, TASK_TAGS, "tag_work", Some(""));
        assert_has_update(&decoded, TASK_TAGS, "tag_next", Some(""));
    }

    #[test]
    fn fixture_dependencies_decodes() {
        let decoded = decode_fixture("generated/dependencies.json");
        assert_has_update(
            &decoded,
            TASK_DEPENDS,
            &format!("dep_{TASK_DEPENDENCY}"),
            Some(""),
        );
    }

    #[test]
    fn fixture_annotations_decodes() {
        let decoded = decode_fixture("generated/annotations.json");
        assert!(decoded.operations.iter().any(|op| matches!(
            op,
            WireOp::Update { uuid, property, value: Some(value), .. }
                if *uuid == TASK_ANNOTATIONS
                    && property.starts_with("annotation_")
                    && value.contains("Phase 2 corpus annotation")
        )));
    }

    #[test]
    fn fixture_uda_operations_decode() {
        let decoded = decode_fixture("generated/udas.json");
        assert_has_update(&decoded, TASK_UDAS, "cmdock_account", Some("WORK"));
        assert_has_update(&decoded, TASK_UDAS, "cmdock_key", Some("WORK-42"));
        assert_has_update(&decoded, TASK_UDAS, "energy", Some("high"));
    }

    #[test]
    fn fixture_multi_op_same_task_decodes() {
        let decoded = decode_fixture("generated/multi_op_same_task.json");
        assert_has_create(&decoded, TASK_MULTI_SAME);
        assert_has_update(&decoded, TASK_MULTI_SAME, "status", Some("pending"));
        assert_has_update(
            &decoded,
            TASK_MULTI_SAME,
            "description",
            Some("fixture multi-op same task"),
        );
        assert_has_update(&decoded, TASK_MULTI_SAME, "project", Some("deep-work"));
        assert_task_op_count_at_least(&decoded, TASK_MULTI_SAME, 4);
    }

    #[test]
    fn fixture_multi_op_multiple_tasks_decodes() {
        let decoded = decode_fixture("generated/multi_op_multiple_tasks.json");
        assert_has_create(&decoded, TASK_MULTI_A);
        assert_has_create(&decoded, TASK_MULTI_B);
        assert_has_update(
            &decoded,
            TASK_MULTI_A,
            "description",
            Some("fixture multi A"),
        );
        assert_has_update(
            &decoded,
            TASK_MULTI_B,
            "description",
            Some("fixture multi B"),
        );
    }

    #[derive(Default)]
    struct CaptureServer {
        segments: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait(?Send)]
    impl taskchampion::Server for CaptureServer {
        async fn add_version(
            &mut self,
            _parent_version_id: VersionId,
            history_segment: Vec<u8>,
        ) -> Result<(AddVersionResult, SnapshotUrgency), taskchampion::Error> {
            self.segments
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(history_segment);
            Ok((AddVersionResult::Ok(Uuid::new_v4()), SnapshotUrgency::None))
        }

        async fn get_child_version(
            &mut self,
            _parent_version_id: VersionId,
        ) -> Result<GetVersionResult, taskchampion::Error> {
            Ok(GetVersionResult::NoSuchVersion)
        }

        async fn add_snapshot(
            &mut self,
            _version_id: VersionId,
            _snapshot: Vec<u8>,
        ) -> Result<(), taskchampion::Error> {
            Ok(())
        }

        async fn get_snapshot(
            &mut self,
        ) -> Result<Option<(VersionId, Vec<u8>)>, taskchampion::Error> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn parses_taskchampion_produced_history_segment() {
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        let mut task = replica.create_task(TASK_UDAS, &mut ops).await.unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_description("golden task".to_string(), &mut ops)
            .unwrap();
        task.set_value("cmdock_account", Some("ALICE".to_string()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();

        let segment = sync_and_capture_one(&mut replica).await;
        let parsed = decode_history_segment(&segment).unwrap();

        assert_has_create(&parsed, TASK_UDAS);
        assert_has_update(&parsed, TASK_UDAS, "description", Some("golden task"));
        assert_has_update(&parsed, TASK_UDAS, "cmdock_account", Some("ALICE"));
    }

    /// Regenerate `tests/fixtures/tc_history/generated/*.json` from public
    /// TaskChampion APIs. This test is ignored because the fixture bytes contain
    /// operation timestamps and should change only during an explicit codec
    /// review (for example, after a TaskChampion bump).
    #[tokio::test]
    #[ignore]
    async fn regenerate_taskchampion_history_fixtures() {
        write_fixture("generated/create.json", generate_create().await);
        write_fixture("generated/delete.json", generate_delete().await);
        write_fixture("generated/update_set.json", generate_update_set().await);
        write_fixture("generated/update_clear.json", generate_update_clear().await);
        write_fixture("generated/tags.json", generate_tags().await);
        write_fixture("generated/dependencies.json", generate_dependencies().await);
        write_fixture("generated/annotations.json", generate_annotations().await);
        write_fixture("generated/udas.json", generate_udas().await);
        write_fixture(
            "generated/multi_op_same_task.json",
            generate_multi_op_same_task().await,
        );
        write_fixture(
            "generated/multi_op_multiple_tasks.json",
            generate_multi_op_multiple_tasks().await,
        );
    }

    async fn generate_create() -> Vec<u8> {
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        let mut task = replica.create_task(TASK_CREATE, &mut ops).await.unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_description("fixture create".to_string(), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_delete() -> Vec<u8> {
        let mut replica = replica_with_synced_task(TASK_DELETE, "fixture delete seed").await;
        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_DELETE).await.unwrap().unwrap();
        task.set_status(Status::Deleted, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_update_set() -> Vec<u8> {
        let mut replica = replica_with_synced_task(TASK_UPDATE, "fixture update seed").await;
        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_UPDATE).await.unwrap().unwrap();
        task.set_value("project", Some("home".to_string()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_update_clear() -> Vec<u8> {
        let mut replica = replica_with_synced_task(TASK_UPDATE, "fixture update seed").await;
        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_UPDATE).await.unwrap().unwrap();
        task.set_value("project", Some("home".to_string()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        let _discard = sync_and_capture_one(&mut replica).await;

        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_UPDATE).await.unwrap().unwrap();
        task.set_value("project", None, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_tags() -> Vec<u8> {
        let mut replica = replica_with_synced_task(TASK_TAGS, "fixture tags seed").await;
        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_TAGS).await.unwrap().unwrap();
        task.add_tag(&Tag::try_from("work").unwrap(), &mut ops)
            .unwrap();
        task.add_tag(&Tag::try_from("next").unwrap(), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_dependencies() -> Vec<u8> {
        let mut replica = replica_with_synced_two_tasks().await;
        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_DEPENDS).await.unwrap().unwrap();
        task.add_dependency(TASK_DEPENDENCY, &mut ops).unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_annotations() -> Vec<u8> {
        let mut replica =
            replica_with_synced_task(TASK_ANNOTATIONS, "fixture annotation seed").await;
        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_ANNOTATIONS).await.unwrap().unwrap();
        task.add_annotation(
            Annotation {
                entry: "2026-05-09T08:00:00Z".parse().unwrap(),
                description: "Phase 2 corpus annotation".to_string(),
            },
            &mut ops,
        )
        .unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_udas() -> Vec<u8> {
        let mut replica = replica_with_synced_task(TASK_UDAS, "fixture UDA seed").await;
        let mut ops = Operations::new();
        let mut task = replica.get_task(TASK_UDAS).await.unwrap().unwrap();
        task.set_value("cmdock_account", Some("WORK".to_string()), &mut ops)
            .unwrap();
        task.set_value("cmdock_key", Some("WORK-42".to_string()), &mut ops)
            .unwrap();
        task.set_value("energy", Some("high".to_string()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_multi_op_same_task() -> Vec<u8> {
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        let mut task = replica
            .create_task(TASK_MULTI_SAME, &mut ops)
            .await
            .unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_description("fixture multi-op same task".to_string(), &mut ops)
            .unwrap();
        task.set_value("project", Some("deep-work".to_string()), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn generate_multi_op_multiple_tasks() -> Vec<u8> {
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        let mut task_a = replica.create_task(TASK_MULTI_A, &mut ops).await.unwrap();
        task_a.set_status(Status::Pending, &mut ops).unwrap();
        task_a
            .set_description("fixture multi A".to_string(), &mut ops)
            .unwrap();
        let mut task_b = replica.create_task(TASK_MULTI_B, &mut ops).await.unwrap();
        task_b.set_status(Status::Pending, &mut ops).unwrap();
        task_b
            .set_description("fixture multi B".to_string(), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        sync_and_capture_one(&mut replica).await
    }

    async fn replica_with_synced_task(uuid: Uuid, description: &str) -> Replica<InMemoryStorage> {
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        let mut task = replica.create_task(uuid, &mut ops).await.unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_description(description.to_string(), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        let _discard = sync_and_capture_one(&mut replica).await;
        replica
    }

    async fn replica_with_synced_two_tasks() -> Replica<InMemoryStorage> {
        let mut replica: Replica<InMemoryStorage> = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        let mut task = replica.create_task(TASK_DEPENDS, &mut ops).await.unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_description("fixture depends task".to_string(), &mut ops)
            .unwrap();
        let mut dependency = replica
            .create_task(TASK_DEPENDENCY, &mut ops)
            .await
            .unwrap();
        dependency.set_status(Status::Pending, &mut ops).unwrap();
        dependency
            .set_description("fixture dependency task".to_string(), &mut ops)
            .unwrap();
        replica.commit_operations(ops).await.unwrap();
        let _discard = sync_and_capture_one(&mut replica).await;
        replica
    }

    async fn sync_and_capture_one(replica: &mut Replica<InMemoryStorage>) -> Vec<u8> {
        let segments = Arc::new(Mutex::new(Vec::new()));
        let capture = CaptureServer {
            segments: Arc::clone(&segments),
        };
        let mut server: Box<dyn taskchampion::Server> = Box::new(capture);
        replica.sync(&mut server, true).await.unwrap();

        let mut guard = segments.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(guard.len(), 1, "TaskChampion should produce one segment");
        guard.pop().unwrap()
    }

    fn assert_has_create(decoded: &WireVersion, uuid: Uuid) {
        assert!(
            decoded
                .operations
                .iter()
                .any(|op| matches!(op, WireOp::Create { uuid: got } if *got == uuid)),
            "expected create op for {uuid} in {decoded:#?}"
        );
    }

    fn assert_has_update(
        decoded: &WireVersion,
        uuid: Uuid,
        expected_property: &str,
        expected_value: Option<&str>,
    ) {
        assert!(
            decoded.operations.iter().any(|op| matches!(
                op,
                WireOp::Update { uuid: got, property, value, .. }
                    if *got == uuid
                        && property == expected_property
                        && value.as_deref() == expected_value
            )),
            "expected update {uuid} {expected_property}={expected_value:?} in {decoded:#?}"
        );
    }

    fn assert_task_op_count_at_least(decoded: &WireVersion, uuid: Uuid, expected_min: usize) {
        let actual = decoded
            .operations
            .iter()
            .filter(|op| op.uuid() == uuid)
            .count();
        assert!(
            actual >= expected_min,
            "expected at least {expected_min} ops for {uuid}, got {actual}: {decoded:#?}"
        );
    }

    fn decode_fixture(relative: &str) -> WireVersion {
        let bytes = load_fixture(relative);
        decode_history_segment(&bytes)
            .unwrap_or_else(|err| panic!("fixture {relative} should decode strictly: {err}"))
    }

    fn load_fixture(relative: &str) -> Vec<u8> {
        fs::read(fixture_path(relative))
            .unwrap_or_else(|err| panic!("read fixture {relative}: {err}"))
    }

    fn write_fixture(relative: &str, bytes: Vec<u8>) {
        let path = fixture_path(relative);
        fs::create_dir_all(path.parent().expect("fixture has parent")).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn fixture_path(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tc_history")
            .join(relative)
    }
}
