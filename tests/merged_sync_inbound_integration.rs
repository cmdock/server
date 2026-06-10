//! Phase 5 merged-sync inbound personal apply integration tests.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::{header, HeaderValue};
use axum::Router;
use axum_test::TestServer;
use cmdock_server::app_state::AppState;
use cmdock_server::merged_sync_gateway::codec::{
    decode_history_segment, encode_history_segment, WireOp, WireVersion,
};
use cmdock_server::merged_sync_gateway::inbound::{
    add_personal_version, GatewayAddVersionOutcome, GatewayVersion,
};
use cmdock_server::merged_sync_gateway::journal::{GatewayJournalState, GatewayRecoveryStatus};
use cmdock_server::merged_sync_gateway::recovery::{recover_all_users, recover_user};
use cmdock_server::store::models::{
    KeyState, MergedSyncJournalTransition, NewMergedSyncJournalAttempt, NewUser,
};
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;
use cmdock_server::tc_sync::storage::{SyncStorage, NIL_VERSION_ID};
use cmdock_server::{health, tc_sync};
use taskchampion::storage::AccessMode;
use taskchampion::{Operations, Replica, SqliteStorage, Status};
use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;
use uuid::Uuid;

static AUDIT_CAPTURE: OnceLock<Arc<Mutex<Vec<HashMap<String, String>>>>> = OnceLock::new();

/// Process-global audit capture for this test binary.
///
/// Uses a single global tracing subscriber (set once), NOT a thread-local
/// `set_default`. Gateway recovery/projection emits audit events from tokio
/// worker threads, `spawn_blocking` pool threads, and the dedicated
/// `replica.sync()` OS thread; a thread-local subscriber installed on the test
/// thread misses those whenever the emission lands off-thread — which the
/// multi-core CI runner triggers reliably while a quiet workstation does not
/// (the `recovery_emits_operator_audit_lifecycle` flake). A global subscriber
/// observes every thread. The buffer is shared across the binary's tests, so
/// each test must filter by its own unique `user_id` (they do); only existence
/// assertions are used, never counts/absence. The returned unit guard preserves
/// the call-site shape.
fn install_scoped_audit_capture() -> (Arc<Mutex<Vec<HashMap<String, String>>>>, ()) {
    let events = AUDIT_CAPTURE
        .get_or_init(|| {
            let events = Arc::new(Mutex::new(Vec::new()));
            let layer = AuditCaptureLayer {
                events: events.clone(),
            };
            let subscriber = tracing_subscriber::registry().with(layer);
            // Set once for the whole test binary; ignore the error if a global
            // default already exists.
            let _ = tracing::subscriber::set_global_default(subscriber);
            events
        })
        .clone();
    (events, ())
}

struct AuditCaptureLayer {
    events: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl<S> Layer<S> for AuditCaptureLayer
where
    S: tracing::Subscriber,
    S: for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        if event.metadata().target() != "audit" {
            return;
        }
        let mut visitor = AuditVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.fields);
    }
}

#[derive(Default)]
struct AuditVisitor {
    fields: HashMap<String, String>,
}

impl Visit for AuditVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

async fn make_state(tmp: &TempDir) -> (AppState, Arc<dyn ConfigStore>, String) {
    let db_path = tmp.path().join("config.sqlite");
    let sqlite = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    let store: Arc<dyn ConfigStore> = sqlite.clone();
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: format!("gateway-{}", Uuid::new_v4()),
            password_hash: "hash".to_string(),
        })
        .await
        .unwrap();
    store.set_user_prefix(&user.id, "PERS").await.unwrap();
    store
        .ensure_personal_task_scope_for_user(&user.id)
        .await
        .unwrap();

    let config = common::test_server_config(tmp.path().to_path_buf());
    let state = AppState::new(store.clone(), sqlite, &config);
    (state, store, user.id)
}

fn segment(ops: Vec<WireOp>) -> Vec<u8> {
    encode_history_segment(&WireVersion { operations: ops }).unwrap()
}

fn client_id_header(client_id: &str) -> (header::HeaderName, HeaderValue) {
    (
        header::HeaderName::from_static("x-client-id"),
        HeaderValue::from_str(client_id).unwrap(),
    )
}

/// Fault-injection helper: bypasses the gateway to create crash-window
/// fixtures. Do not use this for production-shaped happy-path tests.
fn append_raw_merged_version(tmp: &TempDir, user_id: &str, parent: Uuid, body: &[u8]) -> Uuid {
    let storage = SyncStorage::open_merged(&tmp.path().join("users").join(user_id))
        .expect("open merged storage for fault-injection seed");
    storage
        .add_version(parent, body)
        .expect("seed merged version")
        .expect("seed merged version should match current parent")
}

fn child_segment(tmp: &TempDir, user_id: &str, parent: Uuid) -> Vec<u8> {
    let storage = SyncStorage::open_merged(&tmp.path().join("users").join(user_id)).unwrap();
    let (child, parent_known, _) = storage.get_child_version_with_context(parent).unwrap();
    assert!(parent_known, "parent version should remain retained");
    child.expect("expected child version after parent").2
}

async fn create_gateway_journal(
    store: &dyn ConfigStore,
    user_id: &str,
    client_id: &str,
    parent: Uuid,
    body: Vec<u8>,
    target_state: GatewayJournalState,
    merged_version_id: Option<Uuid>,
) -> String {
    let journal_id = Uuid::new_v4().to_string();
    let attempt_id = Uuid::new_v4().to_string();
    store
        .create_merged_sync_journal_attempt(&NewMergedSyncJournalAttempt {
            journal_id: journal_id.clone(),
            user_id: user_id.to_string(),
            client_id: client_id.to_string(),
            attempt_id: attempt_id.clone(),
            parent_version_id: parent.to_string(),
            inbound_history_segment: body,
        })
        .await
        .unwrap();

    if target_state != GatewayJournalState::Received {
        assert!(
            merged_version_id.is_some(),
            "accepted-or-later journal fixtures must carry merged_version_id"
        );
    }

    let mut current = GatewayJournalState::Received;
    for next in [
        GatewayJournalState::MergedVersionAccepted,
        GatewayJournalState::SourcePlanApplied,
        GatewayJournalState::ProjectionAppended,
    ] {
        if current == target_state {
            break;
        }
        // Store persistence carries `merged_version_id` forward after the
        // acceptance transition via COALESCE; later seeded states intentionally
        // pass None here to exercise that contract.
        let merged_version_id_string = if next == GatewayJournalState::MergedVersionAccepted {
            merged_version_id.map(|id| id.to_string())
        } else {
            None
        };
        store
            .transition_merged_sync_journal(MergedSyncJournalTransition {
                journal_id: &journal_id,
                attempt_id: &attempt_id,
                from_state: current,
                to_state: next,
                merged_version_id: merged_version_id_string.as_deref(),
                recovery_status: GatewayRecoveryStatus::Recoverable,
                diagnostic: None,
            })
            .await
            .unwrap()
            .expect("journal transition should match");
        current = next;
    }
    assert_eq!(current, target_state);
    journal_id
}

fn assert_recovery_summary(
    summary: cmdock_server::merged_sync_gateway::recovery::GatewayRecoverySummary,
    inspected: usize,
    recovered: usize,
    skipped_terminal: usize,
) {
    assert_eq!(summary.inspected, inspected);
    assert_eq!(summary.recovered, recovered);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.quarantined, 0);
    assert_eq!(summary.stale, 0);
    assert_eq!(summary.skipped_terminal, skipped_terminal);
}

fn assert_recovery_summary_clean(
    summary: cmdock_server::merged_sync_gateway::recovery::GatewayRecoverySummary,
    recovered: usize,
) {
    assert_recovery_summary(summary, 1, recovered, 0);
}

const HS_CT: &str = "application/vnd.taskchampion.history-segment";

async fn make_http_env(
    tmp: &TempDir,
) -> (
    TestServer,
    AppState,
    Arc<dyn ConfigStore>,
    String,
    String,
    String,
) {
    let (state, store, user_id) = make_state(tmp).await;
    let client_a = Uuid::new_v4().to_string();
    let client_b = Uuid::new_v4().to_string();
    store
        .create_replica(&user_id, &client_a, "test-enc-secret")
        .await
        .unwrap();
    for client_id in [&client_a, &client_b] {
        store
            .create_device(&user_id, client_id, "Test device", None)
            .await
            .unwrap();
    }
    let app = Router::new()
        .merge(health::routes())
        .merge(tc_sync::routes())
        .with_state(state.clone());
    let server = TestServer::new(app);
    (server, state, store, user_id, client_a, client_b)
}

async fn http_add_version(
    server: &TestServer,
    client_id: &str,
    parent: Uuid,
    ops: Vec<WireOp>,
) -> axum_test::TestResponse {
    let (h, v) = client_id_header(client_id);
    server
        .post(&format!("/v1/client/add-version/{parent}"))
        .add_header(h, v)
        .content_type(HS_CT)
        .bytes(segment(ops).into())
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn http_two_tw_clients_same_parent_contend_one_rebases_without_loss() {
    let tmp = TempDir::new().unwrap();
    let (server, _state, _store, user_id, client_a, client_b) = make_http_env(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let t0 = "2026-05-09T00:00:00Z".parse().unwrap();
    let base_resp = http_add_version(
        &server,
        &client_a,
        NIL_VERSION_ID,
        create_task_ops(task_uuid, "base", t0),
    )
    .await;
    base_resp.assert_status_ok();
    let base = Uuid::parse_str(base_resp.header("X-Version-Id").to_str().unwrap()).unwrap();

    let a_ops = vec![update_op(
        task_uuid,
        "description",
        "concurrent client a",
        "2026-05-09T00:01:00Z".parse().unwrap(),
    )];
    let b_ops = vec![update_op(
        task_uuid,
        "project",
        "concurrent-client-b",
        "2026-05-09T00:02:00Z".parse().unwrap(),
    )];
    let (resp_a, resp_b) = tokio::join!(
        http_add_version(&server, &client_a, base, a_ops.clone()),
        http_add_version(&server, &client_b, base, b_ops.clone()),
    );
    let statuses = [resp_a.status_code(), resp_b.status_code()];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == axum::http::StatusCode::OK)
            .count(),
        1,
        "exactly one same-parent contender should append: {statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == axum::http::StatusCode::CONFLICT)
            .count(),
        1,
        "exactly one same-parent contender should receive 409/rebase: {statuses:?}"
    );

    let (winner, loser_ops, loser_client) = if resp_a.status_code() == axum::http::StatusCode::OK {
        (&resp_a, b_ops, &client_b)
    } else {
        (&resp_b, a_ops, &client_a)
    };
    let winner_version = Uuid::parse_str(winner.header("X-Version-Id").to_str().unwrap()).unwrap();
    let conflict = if resp_a.status_code() == axum::http::StatusCode::CONFLICT {
        &resp_a
    } else {
        &resp_b
    };
    assert_eq!(
        conflict.header("X-Parent-Version-Id").to_str().unwrap(),
        winner_version.to_string()
    );

    let retry = http_add_version(&server, loser_client, winner_version, loser_ops).await;
    retry.assert_status_ok();

    let values = read_source_values(&tmp, &user_id, task_uuid).await;
    assert_eq!(
        values["description"].as_deref(),
        Some("concurrent client a")
    );
    assert_eq!(values["project"].as_deref(), Some("concurrent-client-b"));
    assert_no_duplicate_source_tasks(&tmp, &user_id).await;
}

#[tokio::test]
async fn inbound_tw_create_updates_canonical_source_and_finalizes_journal() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let bytes = segment(vec![
        WireOp::Create { uuid: task_uuid },
        WireOp::Update {
            uuid: task_uuid,
            property: "status".to_string(),
            value: Some("pending".to_string()),
            timestamp: chrono::Utc::now(),
        },
        WireOp::Update {
            uuid: task_uuid,
            property: "description".to_string(),
            value: Some("from tw".to_string()),
            timestamp: chrono::Utc::now(),
        },
        WireOp::Update {
            uuid: task_uuid,
            property: "cmdock_key".to_string(),
            value: Some("PERS-999".to_string()),
            timestamp: chrono::Utc::now(),
        },
    ]);

    let outcome = add_personal_version(
        &state,
        GatewayVersion {
            user_id: user_id.clone(),
            client_id: "client-1".to_string(),
            parent_version_id: NIL_VERSION_ID,
            history_segment: bytes,
            request_id: None,
        },
    )
    .await
    .unwrap();

    let (journal_id, version_id) = match outcome {
        GatewayAddVersionOutcome::Accepted {
            journal_id,
            version_id,
        } => (journal_id, version_id),
        other => panic!("expected accepted, got {other:?}"),
    };
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Recovered);
    assert_eq!(
        journal.merged_version_id.as_deref(),
        Some(version_id.to_string().as_str())
    );
    assert!(!journal.inbound_history_segment.is_empty());

    let source_storage = SqliteStorage::new(
        &tmp.path().join("users").join(&user_id),
        AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut source = Replica::new(source_storage);
    let task = source.get_task(task_uuid).await.unwrap().unwrap();
    assert_eq!(task.get_description(), "from tw");
    assert_eq!(task.get_value("cmdock_task_scope"), Some("PERS"));
    assert_eq!(task.get_value("cmdock_key"), Some("PERS-1"));

    let merged_storage =
        SyncStorage::open_merged(&tmp.path().join("users").join(&user_id)).unwrap();
    assert!(merged_storage.version_exists(version_id).unwrap());
}

#[tokio::test]
async fn inbound_invalid_source_operation_rejects_before_merged_acceptance() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let outcome = add_personal_version(
        &state,
        GatewayVersion {
            user_id: user_id.clone(),
            client_id: "client-invalid-source".to_string(),
            parent_version_id: NIL_VERSION_ID,
            history_segment: segment(vec![WireOp::Update {
                uuid: task_uuid,
                property: "status".to_string(),
                value: Some("bogus".to_string()),
                timestamp: chrono::Utc::now(),
            }]),
            request_id: None,
        },
    )
    .await
    .unwrap();

    let journal_id = match outcome {
        GatewayAddVersionOutcome::Rejected { journal_id, code } => {
            assert_eq!(code, "invalid_source_operation");
            journal_id
        }
        other => panic!("expected invalid source rejection, got {other:?}"),
    };
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Failed);
    assert!(journal.merged_version_id.is_none());
    assert_eq!(latest_merged_version(&tmp, &user_id), NIL_VERSION_ID);
}

#[tokio::test]
async fn inbound_forbidden_task_scope_rejects_before_merged_acceptance() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let bytes = segment(vec![
        WireOp::Create { uuid: task_uuid },
        WireOp::Update {
            uuid: task_uuid,
            property: "cmdock_account".to_string(),
            value: Some("TEAM".to_string()),
            timestamp: chrono::Utc::now(),
        },
    ]);

    let outcome = add_personal_version(
        &state,
        GatewayVersion {
            user_id: user_id.clone(),
            client_id: "client-1".to_string(),
            parent_version_id: NIL_VERSION_ID,
            history_segment: bytes,
            request_id: None,
        },
    )
    .await
    .unwrap();

    let journal_id = match outcome {
        GatewayAddVersionOutcome::Rejected { journal_id, code } => {
            assert_eq!(code, "TASK_SCOPE_FORBIDDEN");
            journal_id
        }
        other => panic!("expected rejected, got {other:?}"),
    };
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Failed);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Failed);
    assert!(journal.merged_version_id.is_none());

    let merged_storage =
        SyncStorage::open_merged(&tmp.path().join("users").join(&user_id)).unwrap();
    assert!(!merged_storage.has_versions().unwrap());
}

#[tokio::test]
async fn stale_parent_conflict_does_not_cross_acceptance_boundary() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;

    let first_uuid = Uuid::new_v4();
    let first = add_personal_version(
        &state,
        GatewayVersion {
            user_id: user_id.clone(),
            client_id: "client-1".to_string(),
            parent_version_id: NIL_VERSION_ID,
            history_segment: segment(vec![WireOp::Create { uuid: first_uuid }]),
            request_id: None,
        },
    )
    .await
    .unwrap();
    let first_version = match first {
        GatewayAddVersionOutcome::Accepted { version_id, .. } => version_id,
        other => panic!("expected first accepted, got {other:?}"),
    };

    let second_uuid = Uuid::new_v4();
    let second = add_personal_version(
        &state,
        GatewayVersion {
            user_id: user_id.clone(),
            client_id: "client-1".to_string(),
            parent_version_id: NIL_VERSION_ID,
            history_segment: segment(vec![WireOp::Create { uuid: second_uuid }]),
            request_id: None,
        },
    )
    .await
    .unwrap();

    let journal_id = match second {
        GatewayAddVersionOutcome::ExpectedParentVersion {
            journal_id,
            expected_parent_version_id,
        } => {
            assert_eq!(expected_parent_version_id, first_version);
            journal_id
        }
        other => panic!("expected parent conflict, got {other:?}"),
    };
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Failed);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Failed);
    assert!(journal.merged_version_id.is_none());

    let source_storage = SqliteStorage::new(
        &tmp.path().join("users").join(&user_id),
        AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut source = Replica::new(source_storage);
    assert!(source.get_task(second_uuid).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_received_before_accept_appends_applies_and_finalizes() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "recover received",
        "2026-05-09T01:00:00Z".parse().unwrap(),
    ));
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-r",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::Received,
        None,
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Recovered);
    assert!(journal.merged_version_id.is_some());
    assert_eq!(
        read_source_values(&tmp, &user_id, task_uuid)
            .await
            .get("description")
            .and_then(Clone::clone)
            .as_deref(),
        Some("recover received")
    );
}

#[tokio::test]
async fn recovery_emits_operator_audit_lifecycle() {
    let (audit, _audit_guard) = install_scoped_audit_capture();
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "audit recovery",
        "2026-05-09T01:01:00Z".parse().unwrap(),
    ));
    create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-audit",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::Received,
        None,
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let events = audit.lock().unwrap().clone();
    assert!(events.iter().any(|event| {
        event
            .get("action")
            .is_some_and(|v| v == "merged_sync.recovery_started")
            && event.get("user_id").is_some_and(|v| v == &user_id)
            && event.get("outcome").is_some_and(|v| v == "started")
    }));
    assert!(events.iter().any(|event| {
        event
            .get("action")
            .is_some_and(|v| v == "merged_sync.recovery_finished")
            && event.get("user_id").is_some_and(|v| v == &user_id)
            && event.get("outcome").is_some_and(|v| v == "recovered")
            && event
                .get("merged_version_id")
                .is_some_and(|v| !v.is_empty())
    }));
    assert!(events.iter().any(|event| {
        event
            .get("action")
            .is_some_and(|v| v == "merged_sync.source_apply_succeeded")
            && event.get("user_id").is_some_and(|v| v == &user_id)
            && event.get("outcome").is_some_and(|v| v == "success")
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn recover_all_users_scans_gateway_journals_and_reports_aggregate_counts() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "recover all users",
        "2026-05-09T01:02:00Z".parse().unwrap(),
    ));
    create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-all",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::Received,
        None,
    )
    .await;

    let summary = recover_all_users(&state).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_attempt_transition_does_not_overwrite_recoverable_journal() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "stale attempt guard",
        "2026-05-09T01:03:00Z".parse().unwrap(),
    ));
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-stale",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::Received,
        None,
    )
    .await;

    let stale = store
        .transition_merged_sync_journal(MergedSyncJournalTransition {
            journal_id: &journal_id,
            attempt_id: "wrong-attempt-id",
            from_state: GatewayJournalState::Received,
            to_state: GatewayJournalState::Failed,
            merged_version_id: None,
            recovery_status: GatewayRecoveryStatus::Failed,
            diagnostic: None,
        })
        .await
        .unwrap();
    assert!(stale.is_none(), "stale attempt must not overwrite journal");

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Recovered);
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_accepted_before_source_replays_source_and_finalizes() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "recover accepted",
        "2026-05-09T01:05:00Z".parse().unwrap(),
    ));
    let merged_version_id = append_raw_merged_version(&tmp, &user_id, NIL_VERSION_ID, &body);
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-a",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::MergedVersionAccepted,
        Some(merged_version_id),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Recovered);
    assert_eq!(
        read_source_values(&tmp, &user_id, task_uuid)
            .await
            .get("description")
            .and_then(Clone::clone)
            .as_deref(),
        Some("recover accepted")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_accepted_state_quarantines_when_merged_version_is_missing() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "missing accepted version",
        "2026-05-09T01:06:00Z".parse().unwrap(),
    ));
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-missing-accepted",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::MergedVersionAccepted,
        Some(Uuid::new_v4()),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_eq!(summary.inspected, 1);
    assert_eq!(summary.recovered, 0);
    assert_eq!(summary.quarantined, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Quarantined);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Quarantined);
    assert_eq!(
        journal.diagnostic_code.as_deref(),
        Some("accepted_version_missing_or_mismatch")
    );
}

#[tokio::test]
async fn recovery_terminal_rejection_emits_version_rejected_audit() {
    let (audit, _audit_guard) = install_scoped_audit_capture();
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "missing accepted version audit",
        "2026-05-09T01:06:30Z".parse().unwrap(),
    ));
    create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-missing-audit",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::MergedVersionAccepted,
        Some(Uuid::new_v4()),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_eq!(summary.quarantined, 1);
    let events = audit.lock().unwrap().clone();
    assert!(
        events.iter().any(|event| {
            event
                .get("action")
                .is_some_and(|v| v == "merged_sync.version_rejected")
                && event.get("user_id").is_some_and(|v| v == &user_id)
                && event
                    .get("outcome")
                    .is_some_and(|v| v == "accepted_version_missing_or_mismatch")
        }),
        "expected recovery terminal rejection to emit version_rejected audit; events={events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_accepted_state_quarantines_when_merged_segment_mismatches() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "expected body",
        "2026-05-09T01:07:00Z".parse().unwrap(),
    ));
    let different_body = segment(create_task_ops(
        task_uuid,
        "different retained body",
        "2026-05-09T01:07:01Z".parse().unwrap(),
    ));
    let merged_version_id =
        append_raw_merged_version(&tmp, &user_id, NIL_VERSION_ID, &different_body);
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-mismatch-accepted",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::MergedVersionAccepted,
        Some(merged_version_id),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_eq!(summary.inspected, 1);
    assert_eq!(summary.recovered, 0);
    assert_eq!(summary.quarantined, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Quarantined);
    assert_eq!(
        journal.diagnostic_code.as_deref(),
        Some("accepted_version_missing_or_mismatch")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_attach_before_source_commit_reuses_pending_allocation() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "attach committed before source",
        "2026-05-09T01:07:30Z".parse().unwrap(),
    ));
    let merged_version_id = append_raw_merged_version(&tmp, &user_id, NIL_VERSION_ID, &body);
    let attempt_id = store
        .reserve_task_key_pending(&user_id, "PERS")
        .await
        .unwrap()
        .1;
    store
        .attach_task_uuid_to_pending(&user_id, "PERS", 1, &attempt_id, &task_uuid.to_string())
        .await
        .unwrap();
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-attached-key",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::MergedVersionAccepted,
        Some(merged_version_id),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    let (key, state) = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key, "PERS-1");
    assert_eq!(state, KeyState::Committed);
    let values = read_source_values(&tmp, &user_id, task_uuid).await;
    assert_eq!(
        values["description"].as_deref(),
        Some("attach committed before source")
    );
    assert_eq!(values["cmdock_key"].as_deref(), Some("PERS-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_source_applied_quarantines_when_merged_version_is_missing() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let body = segment(create_task_ops(
        Uuid::new_v4(),
        "source-applied missing merged version",
        "2026-05-09T01:07:45Z".parse().unwrap(),
    ));
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-source-missing-accepted",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::SourcePlanApplied,
        Some(Uuid::new_v4()),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_eq!(summary.quarantined, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Quarantined);
    assert_eq!(
        journal.diagnostic_code.as_deref(),
        Some("accepted_version_missing_or_mismatch")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_source_commit_before_key_commit_finalizes_pending_allocation() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "source committed before key finalizer",
        "2026-05-09T01:08:00Z".parse().unwrap(),
    ));
    let merged_version_id = append_raw_merged_version(&tmp, &user_id, NIL_VERSION_ID, &body);

    let attempt_id = store
        .reserve_task_key_pending(&user_id, "PERS")
        .await
        .unwrap()
        .1;
    store
        .attach_task_uuid_to_pending(&user_id, "PERS", 1, &attempt_id, &task_uuid.to_string())
        .await
        .unwrap();
    {
        let source_storage = SqliteStorage::new(
            &tmp.path().join("users").join(&user_id),
            AccessMode::ReadWrite,
            true,
        )
        .await
        .unwrap();
        let mut source = Replica::new(source_storage);
        let mut ops = Operations::new();
        let mut task = source.create_task(task_uuid, &mut ops).await.unwrap();
        task.set_status(Status::Pending, &mut ops).unwrap();
        task.set_description(
            "source committed before key finalizer".to_string(),
            &mut ops,
        )
        .unwrap();
        task.set_value("cmdock_account", Some("PERS".to_string()), &mut ops)
            .unwrap();
        task.set_value("cmdock_key", Some("PERS-1".to_string()), &mut ops)
            .unwrap();
        source.commit_operations(ops).await.unwrap();
    }

    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-pending-key",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::MergedVersionAccepted,
        Some(merged_version_id),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    let (key, state) = store
        .lookup_task_key_by_uuid(&user_id, &task_uuid.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key, "PERS-1");
    assert_eq!(state, KeyState::Committed);
    let values = read_source_values(&tmp, &user_id, task_uuid).await;
    assert_eq!(values["cmdock_account"].as_deref(), Some("PERS"));
    assert_eq!(values["cmdock_key"].as_deref(), Some("PERS-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_scan_nonterminal_rows_ignores_terminal_display_limit_noise() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let body = segment(create_task_ops(
        Uuid::new_v4(),
        "old recoverable survives terminal noise",
        "2026-05-09T01:09:00Z".parse().unwrap(),
    ));
    create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-old-recoverable",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::Received,
        None,
    )
    .await;

    let conn = rusqlite::Connection::open(tmp.path().join("config.sqlite")).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    for i in 0..10_005 {
        tx.execute(
            "INSERT INTO merged_sync_journal (
                journal_id, user_id, client_id, attempt_id, parent_version_id,
                inbound_history_segment, state, recovery_status, diagnostic_code,
                diagnostic_message, finalized_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, X'', 'failed', 'failed', 'noise', 'terminal noise', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                user_id,
                format!("terminal-client-{i}"),
                Uuid::new_v4().to_string(),
                NIL_VERSION_ID.to_string(),
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_source_before_projection_appends_correction_and_finalizes() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let client_id = "client-c".to_string();
    let task_uuid = Uuid::new_v4();
    let base = add_version_expect_accepted(
        &state,
        &user_id,
        &client_id,
        NIL_VERSION_ID,
        create_task_ops(
            task_uuid,
            "needs correction",
            "2026-05-09T01:10:00Z".parse().unwrap(),
        ),
    )
    .await;
    let tamper = segment(vec![update_op(
        task_uuid,
        "cmdock_key",
        "PERS-999",
        "2026-05-09T01:11:00Z".parse().unwrap(),
    )]);
    let tamper_version = append_raw_merged_version(&tmp, &user_id, base, &tamper);
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        &client_id,
        base,
        tamper,
        GatewayJournalState::SourcePlanApplied,
        Some(tamper_version),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Recovered);
    let latest = latest_merged_version(&tmp, &user_id);
    assert_ne!(
        latest, tamper_version,
        "correction should append a new merged version"
    );
    let corrected_segment = child_segment(&tmp, &user_id, tamper_version);
    let corrected = decode_history_segment(&corrected_segment).unwrap();
    assert!(
        corrected.operations.iter().any(|op| matches!(
            op,
            WireOp::Update { uuid, property, value, .. }
                if *uuid == task_uuid && property == "cmdock_key" && value.as_deref() == Some("PERS-1")
        )),
        "corrective projection must restore canonical cmdock_key"
    );
    // cmdock_account is no longer canonical at beta — correction does not write it.
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_projection_before_finalize_finalizes_only() {
    let tmp = TempDir::new().unwrap();
    let (state, store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let body = segment(create_task_ops(
        task_uuid,
        "already projected",
        "2026-05-09T01:15:00Z".parse().unwrap(),
    ));
    let merged_version_id = append_raw_merged_version(&tmp, &user_id, NIL_VERSION_ID, &body);
    let journal_id = create_gateway_journal(
        store.as_ref(),
        &user_id,
        "client-p",
        NIL_VERSION_ID,
        body,
        GatewayJournalState::ProjectionAppended,
        Some(merged_version_id),
    )
    .await;

    let summary = recover_user(&state, &user_id).await.unwrap();
    assert_recovery_summary_clean(summary, 1);
    let journal = store
        .get_merged_sync_journal(&journal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.state, GatewayJournalState::Finalized);
    assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Recovered);
}

async fn add_version_expect_accepted(
    state: &AppState,
    user_id: &str,
    client_id: &str,
    parent_version_id: Uuid,
    ops: Vec<WireOp>,
) -> Uuid {
    match add_personal_version(
        state,
        GatewayVersion {
            user_id: user_id.to_string(),
            client_id: client_id.to_string(),
            parent_version_id,
            history_segment: segment(ops),
            request_id: None,
        },
    )
    .await
    .unwrap()
    {
        GatewayAddVersionOutcome::Accepted { version_id, .. } => version_id,
        other => panic!("expected accepted add-version, got {other:?}"),
    }
}

async fn add_version_expect_conflict(
    state: &AppState,
    user_id: &str,
    client_id: &str,
    parent_version_id: Uuid,
    ops: Vec<WireOp>,
    expected_parent: Uuid,
) {
    match add_personal_version(
        state,
        GatewayVersion {
            user_id: user_id.to_string(),
            client_id: client_id.to_string(),
            parent_version_id,
            history_segment: segment(ops),
            request_id: None,
        },
    )
    .await
    .unwrap()
    {
        GatewayAddVersionOutcome::ExpectedParentVersion {
            expected_parent_version_id,
            ..
        } => assert_eq!(expected_parent_version_id, expected_parent),
        other => panic!("expected parent conflict, got {other:?}"),
    }
}

fn create_task_ops(
    uuid: Uuid,
    description: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Vec<WireOp> {
    vec![
        WireOp::Create { uuid },
        WireOp::Update {
            uuid,
            property: "status".to_string(),
            value: Some("pending".to_string()),
            timestamp,
        },
        WireOp::Update {
            uuid,
            property: "description".to_string(),
            value: Some(description.to_string()),
            timestamp,
        },
    ]
}

fn update_op(
    uuid: Uuid,
    property: &str,
    value: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> WireOp {
    WireOp::Update {
        uuid,
        property: property.to_string(),
        value: Some(value.to_string()),
        timestamp,
    }
}

async fn read_source_values(
    tmp: &TempDir,
    user_id: &str,
    uuid: Uuid,
) -> std::collections::HashMap<String, Option<String>> {
    let source_storage = SqliteStorage::new(
        &tmp.path().join("users").join(user_id),
        AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut source = Replica::new(source_storage);
    let task = source.get_task(uuid).await.unwrap().unwrap();
    [
        "description",
        "project",
        "priority",
        "cmdock_account",
        "cmdock_key",
    ]
    .into_iter()
    .map(|property| {
        (
            property.to_string(),
            task.get_value(property).map(ToOwned::to_owned),
        )
    })
    .collect()
}

async fn source_write_description(state: &AppState, user_id: &str, uuid: Uuid, description: &str) {
    let mutation_lock = state.recovery_runtime.task_mutation_lock(user_id);
    let _guard = mutation_lock.lock().await;
    let rep_arc = state.replica_manager.get_replica(user_id).await.unwrap();
    let mut rep = rep_arc.lock().await;
    let mut task = rep.get_task(uuid).await.unwrap().unwrap();
    let mut ops = Operations::new();
    task.set_description(description.to_string(), &mut ops)
        .unwrap();
    rep.commit_operations(ops).await.unwrap();
}

async fn source_write_project(state: &AppState, user_id: &str, uuid: Uuid, project: &str) {
    let mutation_lock = state.recovery_runtime.task_mutation_lock(user_id);
    let _guard = mutation_lock.lock().await;
    let rep_arc = state.replica_manager.get_replica(user_id).await.unwrap();
    let mut rep = rep_arc.lock().await;
    let mut task = rep.get_task(uuid).await.unwrap().unwrap();
    let mut ops = Operations::new();
    task.set_value("project", Some(project.to_string()), &mut ops)
        .unwrap();
    rep.commit_operations(ops).await.unwrap();
}

async fn purge_source_task_direct(state: &AppState, user_id: &str, uuid: Uuid) {
    let rep_arc = state.replica_manager.get_replica(user_id).await.unwrap();
    let mut rep = rep_arc.lock().await;
    let mut task_data = rep.get_task_data(uuid).await.unwrap().unwrap();
    let mut ops = Operations::new();
    task_data.delete(&mut ops);
    rep.commit_operations(ops).await.unwrap();
}

fn latest_merged_version(tmp: &TempDir, user_id: &str) -> Uuid {
    SyncStorage::open_merged(&tmp.path().join("users").join(user_id))
        .unwrap()
        .get_latest_version_id()
        .unwrap()
}

async fn wait_for_merged_head_change(tmp: &TempDir, user_id: &str, old_head: Uuid) -> Uuid {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let latest = latest_merged_version(tmp, user_id);
        if latest != old_head {
            return latest;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for merged head to advance"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn assert_no_duplicate_source_tasks(tmp: &TempDir, user_id: &str) {
    let source_storage = SqliteStorage::new(
        &tmp.path().join("users").join(user_id),
        AccessMode::ReadWrite,
        true,
    )
    .await
    .unwrap();
    let mut source = Replica::new(source_storage);
    let tasks = source.all_tasks().await.unwrap();
    let unique: std::collections::HashSet<_> = tasks.keys().copied().collect();
    assert_eq!(
        tasks.len(),
        unique.len(),
        "source replica has duplicate UUIDs"
    );
}

#[tokio::test]
async fn two_tw_clients_same_base_different_properties_rebase_without_lost_write() {
    let tmp = TempDir::new().unwrap();
    let (state, _store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let t0 = "2026-05-09T00:00:00Z".parse().unwrap();
    let base = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        NIL_VERSION_ID,
        create_task_ops(task_uuid, "base", t0),
    )
    .await;

    let t1 = "2026-05-09T00:01:00Z".parse().unwrap();
    let t2 = "2026-05-09T00:02:00Z".parse().unwrap();
    let a_ops = vec![update_op(task_uuid, "description", "from client a", t1)];
    let b_ops = vec![update_op(task_uuid, "project", "from-client-b", t2)];
    let head_after_a = add_version_expect_accepted(&state, &user_id, "client-a", base, a_ops).await;
    add_version_expect_conflict(
        &state,
        &user_id,
        "client-b",
        base,
        b_ops.clone(),
        head_after_a,
    )
    .await;
    add_version_expect_accepted(&state, &user_id, "client-b", head_after_a, b_ops).await;

    let values = read_source_values(&tmp, &user_id, task_uuid).await;
    assert_eq!(values["description"].as_deref(), Some("from client a"));
    assert_eq!(values["project"].as_deref(), Some("from-client-b"));
    assert_no_duplicate_source_tasks(&tmp, &user_id).await;
}

#[tokio::test]
async fn two_tw_clients_same_base_same_property_timestamp_ordering_is_deterministic() {
    let tmp = TempDir::new().unwrap();
    let (state, _store, user_id) = make_state(&tmp).await;
    let older = "2026-05-09T00:01:00Z".parse().unwrap();
    let newer = "2026-05-09T00:02:00Z".parse().unwrap();

    let first_uuid = Uuid::new_v4();
    let base = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        NIL_VERSION_ID,
        create_task_ops(first_uuid, "base", older),
    )
    .await;
    let h1 = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        base,
        vec![update_op(first_uuid, "description", "older", older)],
    )
    .await;
    let head_after_newer = add_version_expect_accepted(
        &state,
        &user_id,
        "client-b",
        h1,
        vec![update_op(first_uuid, "description", "newer", newer)],
    )
    .await;
    let values = read_source_values(&tmp, &user_id, first_uuid).await;
    assert_eq!(values["description"].as_deref(), Some("newer"));

    let second_uuid = Uuid::new_v4();
    let head = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        head_after_newer,
        create_task_ops(second_uuid, "base", older),
    )
    .await;
    let h2 = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        head,
        vec![update_op(second_uuid, "description", "newer-first", newer)],
    )
    .await;
    add_version_expect_accepted(
        &state,
        &user_id,
        "client-b",
        h2,
        vec![update_op(
            second_uuid,
            "description",
            "older-rebased",
            older,
        )],
    )
    .await;
    let values = read_source_values(&tmp, &user_id, second_uuid).await;
    assert_eq!(
        values["description"].as_deref(),
        Some("newer-first"),
        "an older rebased TW timestamp must not overwrite a newer source value"
    );

    let third_uuid = Uuid::new_v4();
    let equal_base = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        latest_merged_version(&tmp, &user_id),
        create_task_ops(third_uuid, "base", older),
    )
    .await;
    let equal_a = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        equal_base,
        vec![update_op(third_uuid, "description", "equal-a", newer)],
    )
    .await;
    add_version_expect_accepted(
        &state,
        &user_id,
        "client-b",
        equal_a,
        vec![update_op(third_uuid, "description", "equal-b", newer)],
    )
    .await;
    let values = read_source_values(&tmp, &user_id, third_uuid).await;
    assert_eq!(
        values["description"].as_deref(),
        Some("equal-b"),
        "equal TW timestamps use deterministic rebased acceptance order"
    );
}

#[tokio::test]
async fn source_writer_and_tw_writer_same_property_race_uses_timestamp_policy() {
    let tmp = TempDir::new().unwrap();
    let (state, _store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let t0 = "2026-05-09T00:00:00Z".parse().unwrap();
    let base = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        NIL_VERSION_ID,
        create_task_ops(task_uuid, "base", t0),
    )
    .await;

    source_write_description(&state, &user_id, task_uuid, "source description").await;
    let projection =
        cmdock_server::merged_sync_gateway::projection::project_personal_now(&state, &user_id)
            .await
            .unwrap();
    assert!(projection.changed);
    let projected_head = latest_merged_version(&tmp, &user_id);

    let stale_tw_ops = vec![update_op(
        task_uuid,
        "description",
        "older offline tw description",
        "2000-01-01T00:01:00Z".parse().unwrap(),
    )];
    add_version_expect_conflict(
        &state,
        &user_id,
        "client-a",
        base,
        stale_tw_ops.clone(),
        projected_head,
    )
    .await;
    add_version_expect_accepted(&state, &user_id, "client-a", projected_head, stale_tw_ops).await;

    let values = read_source_values(&tmp, &user_id, task_uuid).await;
    assert_eq!(
        values["description"].as_deref(),
        Some("source description"),
        "older rebased TW same-property write must not overwrite a newer source/REST write"
    );
    assert_no_duplicate_source_tasks(&tmp, &user_id).await;
}

#[tokio::test]
async fn source_writer_and_tw_writer_different_property_race_converges_after_rebase() {
    let tmp = TempDir::new().unwrap();
    let (state, _store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let t0 = "2026-05-09T00:00:00Z".parse().unwrap();
    let base = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        NIL_VERSION_ID,
        create_task_ops(task_uuid, "base", t0),
    )
    .await;

    source_write_project(&state, &user_id, task_uuid, "rest-project").await;
    let projection =
        cmdock_server::merged_sync_gateway::projection::project_personal_now(&state, &user_id)
            .await
            .unwrap();
    assert!(
        projection.changed,
        "REST/source write should append a merged projection"
    );
    let projected_head = latest_merged_version(&tmp, &user_id);
    assert_ne!(
        projected_head, base,
        "REST/source write should append a merged projection"
    );

    let offline_tw_ops = vec![update_op(
        task_uuid,
        "description",
        "offline tw description",
        "2026-05-09T00:03:00Z".parse().unwrap(),
    )];
    add_version_expect_conflict(
        &state,
        &user_id,
        "client-a",
        base,
        offline_tw_ops.clone(),
        projected_head,
    )
    .await;
    add_version_expect_accepted(&state, &user_id, "client-a", projected_head, offline_tw_ops).await;

    let values = read_source_values(&tmp, &user_id, task_uuid).await;
    assert_eq!(values["project"].as_deref(), Some("rest-project"));
    assert_eq!(
        values["description"].as_deref(),
        Some("offline tw description")
    );
    assert_no_duplicate_source_tasks(&tmp, &user_id).await;
}

#[tokio::test]
async fn mid_gateway_source_update_while_inbound_is_journaled_converges() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let tmp = TempDir::new().unwrap();
            let (state, _store, user_id) = make_state(&tmp).await;
            let task_uuid = Uuid::new_v4();
            let t0 = "2026-05-09T00:00:00Z".parse().unwrap();
            let base = add_version_expect_accepted(
                &state,
                &user_id,
                "client-a",
                NIL_VERSION_ID,
                create_task_ops(task_uuid, "base", t0),
            )
            .await;

            let mutation_lock = state.recovery_runtime.task_mutation_lock(&user_id);
            let mutation_guard = mutation_lock.lock().await;

            let source_state = state.clone();
            let source_user = user_id.clone();
            let source_task = task_uuid;
            let source_handle = tokio::task::spawn_local(async move {
                source_write_project(
                    &source_state,
                    &source_user,
                    source_task,
                    "mid-source-project",
                )
                .await;
            });
            tokio::task::yield_now().await;

            let inbound_state = state.clone();
            let inbound_user = user_id.clone();
            let inbound_handle = tokio::task::spawn_local(async move {
                add_version_expect_accepted(
                    &inbound_state,
                    &inbound_user,
                    "client-a",
                    base,
                    vec![update_op(
                        task_uuid,
                        "description",
                        "inbound while source queued",
                        "2026-05-09T00:03:00Z".parse().unwrap(),
                    )],
                )
                .await;
            });

            let accepted_head = wait_for_merged_head_change(&tmp, &user_id, base).await;
            assert_ne!(
                accepted_head, base,
                "inbound version should be journaled/accepted before source apply can acquire the mutation lock"
            );

            drop(mutation_guard);
            source_handle.await.unwrap();
            inbound_handle.await.unwrap();

            let values = read_source_values(&tmp, &user_id, task_uuid).await;
            assert_eq!(values["project"].as_deref(), Some("mid-source-project"));
            assert_eq!(
                values["description"].as_deref(),
                Some("inbound while source queued")
            );
            cmdock_server::merged_sync_gateway::projection::project_personal_now(&state, &user_id)
                .await
                .unwrap();
            assert_no_duplicate_source_tasks(&tmp, &user_id).await;
        })
        .await;
}

#[tokio::test]
async fn post_accept_source_disappearance_quarantines_journal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let tmp = TempDir::new().unwrap();
            let (state, store, user_id) = make_state(&tmp).await;
            let task_uuid = Uuid::new_v4();
            let t0 = "2026-05-09T00:00:00Z".parse().unwrap();
            let base = add_version_expect_accepted(
                &state,
                &user_id,
                "client-a",
                NIL_VERSION_ID,
                create_task_ops(task_uuid, "base", t0),
            )
            .await;

            // Hold the canonical mutation lock so inbound can validate source
            // truth and append to the merged chain, then must wait before
            // applying to source. This pins the documented validate/apply
            // TOCTOU policy without depending on scheduler luck.
            let mutation_lock = state.recovery_runtime.task_mutation_lock(&user_id);
            let mutation_guard = mutation_lock.lock().await;

            let inbound_state = state.clone();
            let inbound_user = user_id.clone();
            let inbound_handle = tokio::task::spawn_local(async move {
                add_personal_version(
                    &inbound_state,
                    GatewayVersion {
                        user_id: inbound_user,
                        client_id: "client-a".to_string(),
                        parent_version_id: base,
                        history_segment: segment(vec![update_op(
                            task_uuid,
                            "description",
                            "inbound after disappearing source",
                            "2026-05-09T00:04:00Z".parse().unwrap(),
                        )]),
                        request_id: None,
                    },
                )
                .await
            });

            let accepted_head = wait_for_merged_head_change(&tmp, &user_id, base).await;
            purge_source_task_direct(&state, &user_id, task_uuid).await;
            drop(mutation_guard);

            let err = inbound_handle
                .await
                .unwrap()
                .expect_err("post-accept source disappearance should quarantine");
            assert!(
                err.to_string().contains("apply source plan"),
                "unexpected error: {err:#}"
            );

            let rows = store
                .list_merged_sync_journal_for_user(&user_id, 1)
                .await
                .unwrap();
            let journal = rows.first().expect("expected inbound journal row");
            assert_eq!(journal.state, GatewayJournalState::Quarantined);
            assert_eq!(journal.recovery_status, GatewayRecoveryStatus::Quarantined);
            assert_eq!(
                journal.diagnostic_code.as_deref(),
                Some("source_apply_failed")
            );
            assert_eq!(
                journal.merged_version_id.as_deref(),
                Some(accepted_head.to_string().as_str())
            );
        })
        .await;
}

#[tokio::test]
async fn future_dated_tw_timestamp_wins_but_future_key_tamper_is_corrected() {
    let tmp = TempDir::new().unwrap();
    let (state, _store, user_id) = make_state(&tmp).await;
    let task_uuid = Uuid::new_v4();
    let t0 = "2026-05-09T00:00:00Z".parse().unwrap();
    let base = add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        NIL_VERSION_ID,
        create_task_ops(task_uuid, "base", t0),
    )
    .await;

    let future = "2099-01-01T00:00:00Z".parse().unwrap();
    add_version_expect_accepted(
        &state,
        &user_id,
        "client-a",
        base,
        vec![
            update_op(task_uuid, "description", "future value", future),
            update_op(task_uuid, "cmdock_key", "PERS-999999", future),
            update_op(task_uuid, "cmdock_account", "PERS", future),
        ],
    )
    .await;
    let corrected_head = latest_merged_version(&tmp, &user_id);
    add_version_expect_accepted(
        &state,
        &user_id,
        "client-b",
        corrected_head,
        vec![update_op(
            task_uuid,
            "description",
            "older replay after future",
            "2026-05-09T00:04:00Z".parse().unwrap(),
        )],
    )
    .await;

    let values = read_source_values(&tmp, &user_id, task_uuid).await;
    assert_eq!(values["description"].as_deref(), Some("future value"));
    // cmdock_account ops from inbound versions are dropped; server no longer stamps it.
    assert_eq!(values["cmdock_account"].as_deref(), None);
    assert_eq!(
        values["cmdock_key"].as_deref(),
        Some("PERS-1"),
        "future-dated user cmdock_key input is corrective input, not source truth"
    );
}
