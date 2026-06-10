//! Goose load test binary for cmdock-server.
//!
//! Tests concurrency with multiple users, canonical replicas, and registered
//! sync devices.
//!
//! REST API scenarios:
//!   ReadHeavy (weight 7)  — list tasks, get views, get app-config, healthz
//!   WriteHeavy (weight 3) — add task, complete task, delete task (full lifecycle)
//!   Mixed (weight 4)      — add, list, modify, list, delete (full cycle)
//!   Contention (weight 2) — many VUs modify the same "hot" task (write-write conflict)
//!
//! Sync protocol scenarios:
//!   SyncWrite (weight 3)  — add-version to build a chain (simulates `task sync` push)
//!   SyncRead (weight 2)   — get-child-version to traverse chain (simulates `task sync` pull)
//!   SyncMixed (weight 3)  — add versions + read back + occasional snapshot
//!
//! Bridge sync scenario (requires CMDOCK_MASTER_KEY):
//!   BridgeSync (weight 2) — TC push then REST read (measures bridge latency overhead)
//!
//! Note: sync writes use valid encrypted TaskChampion history segments derived
//! from registered device credentials. Snapshot traffic is read-only in this
//! harness; snapshot propagation correctness is covered by integration tests.
//!
//! Profiles:
//!   mixed                     - personal users plus one shared team user
//!   personal-only             - isolated users only
//!   team-contention           - all VUs share one hot team user/device
//!   multi-device-single-user  - one user with many registered devices
//!
//! Profiles also narrow the scenario mix so small runs still hit the intended
//! behaviour. For example, `multi-device-single-user` prioritises one-user/
//! many-device pressure with read-heavy sync traffic and occasional writers,
//! rather than having every device continuously push versions.
//!
//! Environment:
//!   TC_LOAD_TOKENS_FILE — Path to file, one line per user:
//!                         `type:bearer_token:client_id:sync_secret`
//!   TC_LOAD_TOKEN       — Fallback: single bearer token for all VUs (legacy, no sync)
//!   TC_LOAD_PROFILE     — mixed | personal-only | team-contention | multi-device-single-user

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use goose::goose::GooseResponse;
use goose::prelude::*;
use tokio::sync::RwLock;
use uuid::Uuid;

use cmdock_server::tc_sync::crypto::SyncCryptor;

/// Session data stored per virtual user.
#[derive(Debug, Clone)]
struct UserSession {
    token: String,
    #[allow(dead_code)]
    user_type: String,
    /// Last created task UUID (bounded — only keeps the most recent).
    last_created_uuid: Option<String>,
    /// Sync client ID for TC sync protocol (X-Client-Id header).
    sync_client_id: Option<String>,
    /// Device sync secret as emitted by admin device create.
    sync_secret: Option<String>,
    /// Latest version ID in the sync chain (for building the chain incrementally).
    sync_latest_version: Option<String>,
}

/// Global counter for token assignment.
static USER_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Global counter for contention iterations (incremented each modify call).
static CONTENTION_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Shared "hot task" UUID for the Contention scenario.
/// Created by the first Contention VU, used by all others.
static HOT_TASK_UUID: OnceLock<RwLock<Option<String>>> = OnceLock::new();

/// Loaded tokens, shared across all VUs via OnceLock (avoids passing via env var).
static LOADED_TOKENS: OnceLock<Vec<TokenEntry>> = OnceLock::new();
static LOAD_PROFILE: OnceLock<LoadProfile> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadProfile {
    Mixed,
    PersonalOnly,
    TeamContention,
    MultiDeviceSingleUser,
}

impl LoadProfile {
    fn from_env() -> Self {
        match std::env::var("TC_LOAD_PROFILE")
            .unwrap_or_else(|_| "mixed".to_string())
            .as_str()
        {
            "mixed" => Self::Mixed,
            "personal-only" => Self::PersonalOnly,
            "team-contention" => Self::TeamContention,
            "multi-device-single-user" => Self::MultiDeviceSingleUser,
            other => panic!("Unsupported TC_LOAD_PROFILE: {other}"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::PersonalOnly => "personal-only",
            Self::TeamContention => "team-contention",
            Self::MultiDeviceSingleUser => "multi-device-single-user",
        }
    }

    fn include_contention(self) -> bool {
        !matches!(self, Self::PersonalOnly)
    }

    fn include_read_heavy(self) -> bool {
        !matches!(self, Self::MultiDeviceSingleUser | Self::TeamContention)
    }

    fn include_write_heavy(self) -> bool {
        !matches!(self, Self::MultiDeviceSingleUser)
    }

    fn include_mixed(self) -> bool {
        !matches!(self, Self::TeamContention)
    }

    fn include_sync_mixed(self) -> bool {
        !matches!(self, Self::MultiDeviceSingleUser | Self::TeamContention)
    }

    fn include_bridge_sync(self) -> bool {
        true
    }
}

/// Token entry loaded from file.
/// Every entry carries REST auth and, when present, a registered device's sync
/// credentials.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TokenEntry {
    user_type: String,
    bearer_token: String,
    /// Only present for sync users (X-Client-Id header).
    sync_client_id: Option<String>,
    /// Only present for sync users (hex string used as PBKDF2 input).
    sync_secret: Option<String>,
}

/// Load and validate tokens from file or env var.
///
/// Token file format: `type:bearer_token:client_id:sync_secret`
/// Every user has both REST (bearer) and sync (client_id) capabilities,
/// so any VU can run any scenario regardless of Goose's weight-based assignment.
///
/// Legacy formats (`type:token` without client_id) are still accepted for
/// backwards compat — those VUs will skip sync-protocol scenarios.
fn load_tokens() -> Vec<TokenEntry> {
    if let Ok(path) = std::env::var("TC_LOAD_TOKENS_FILE") {
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Cannot read tokens file {path}: {e}"));
        let tokens: Vec<TokenEntry> = contents
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|line| {
                let parts: Vec<&str> = line.splitn(4, ':').collect();
                match parts.as_slice() {
                    [typ, token, client_id, sync_secret] => TokenEntry {
                        user_type: typ.to_string(),
                        bearer_token: token.to_string(),
                        sync_client_id: Some(client_id.to_string()),
                        sync_secret: Some(sync_secret.to_string()),
                    },
                    [typ, token] => TokenEntry {
                        user_type: typ.to_string(),
                        bearer_token: token.to_string(),
                        sync_client_id: None,
                        sync_secret: None,
                    },
                    [typ, token, client_id] => TokenEntry {
                        user_type: typ.to_string(),
                        bearer_token: token.to_string(),
                        sync_client_id: Some(client_id.to_string()),
                        sync_secret: None,
                    },
                    _ => panic!("Invalid token line: {line}"),
                }
            })
            .collect();
        assert!(
            !tokens.is_empty(),
            "Tokens file {path} contains no valid tokens"
        );
        return tokens;
    }

    if let Ok(token) = std::env::var("TC_LOAD_TOKEN") {
        assert!(!token.is_empty(), "TC_LOAD_TOKEN is empty");
        return vec![TokenEntry {
            user_type: "single".to_string(),
            bearer_token: token,
            sync_client_id: None,
            sync_secret: None,
        }];
    }

    panic!("Set TC_LOAD_TOKENS_FILE or TC_LOAD_TOKEN");
}

#[tokio::main]
async fn main() -> Result<(), GooseError> {
    let profile = LoadProfile::from_env();
    let tokens = load_tokens();
    let personal_count = tokens.iter().filter(|t| t.user_type == "personal").count();
    let team_count = tokens.iter().filter(|t| t.user_type == "team").count();
    let multi_device_count = tokens
        .iter()
        .filter(|t| t.user_type == "multi_device")
        .count();
    let sync_capable = tokens.iter().filter(|t| t.sync_client_id.is_some()).count();
    eprintln!(
        "Loaded {} tokens ({personal_count} personal, {team_count} team, {multi_device_count} multi-device, {sync_capable} sync-capable) for profile {}",
        tokens.len()
        ,
        profile.as_str()
    );

    LOADED_TOKENS
        .set(tokens.clone())
        .expect("tokens already set");
    LOAD_PROFILE.set(profile).expect("profile already set");

    // Initialise hot task lock
    HOT_TASK_UUID.get_or_init(|| RwLock::new(None));

    let mut attack = GooseAttack::initialize()?;
    if profile.include_read_heavy() {
        let read_weight = match profile {
            LoadProfile::Mixed | LoadProfile::PersonalOnly => 7,
            LoadProfile::TeamContention | LoadProfile::MultiDeviceSingleUser => 0,
        };
        if read_weight > 0 {
            attack = attack.register_scenario(
                scenario!("ReadHeavy")
                    .set_weight(read_weight)?
                    .register_transaction(transaction!(setup_session).set_on_start())
                    .register_transaction(transaction!(tx_list_tasks))
                    .register_transaction(transaction!(tx_get_views))
                    .register_transaction(transaction!(tx_get_app_config))
                    .register_transaction(transaction!(tx_healthz)),
            );
        }
    }
    if profile.include_write_heavy() {
        attack = attack.register_scenario(
            scenario!("WriteHeavy")
                .set_weight(3)?
                .register_transaction(transaction!(setup_session).set_on_start())
                .register_transaction(transaction!(tx_add_complete_delete)),
        );
    }
    if profile.include_mixed() {
        let mixed_weight = match profile {
            LoadProfile::MultiDeviceSingleUser => 4,
            _ => 4,
        };
        attack = attack.register_scenario(
            scenario!("Mixed")
                .set_weight(mixed_weight)?
                .register_transaction(transaction!(setup_session).set_on_start())
                .register_transaction(transaction!(tx_add_task))
                .register_transaction(transaction!(tx_list_tasks))
                .register_transaction(transaction!(tx_modify_latest))
                .register_transaction(transaction!(tx_list_tasks))
                .register_transaction(transaction!(tx_delete_latest)),
        );
    }
    if profile.include_contention() {
        let contention_weight = match profile {
            LoadProfile::TeamContention => 5,
            LoadProfile::Mixed => 2,
            _ => 0,
        };
        if contention_weight > 0 {
            attack = attack.register_scenario(
                scenario!("Contention")
                    .set_weight(contention_weight)?
                    .register_transaction(transaction!(setup_session).set_on_start())
                    .register_transaction(transaction!(tx_contention_ensure_hot_task))
                    .register_transaction(transaction!(tx_contention_modify)),
            );
        }
    }
    attack = attack.register_scenario(
        scenario!("SyncWrite")
            .set_weight(match profile {
                LoadProfile::TeamContention => 4,
                LoadProfile::MultiDeviceSingleUser => 2,
                _ => 3,
            })?
            .register_transaction(transaction!(setup_session).set_on_start())
            .register_transaction(transaction!(tx_sync_add_version)),
    );
    attack = attack.register_scenario(
        scenario!("SyncRead")
            .set_weight(match profile {
                LoadProfile::TeamContention => 3,
                LoadProfile::MultiDeviceSingleUser => 4,
                _ => 2,
            })?
            .register_transaction(transaction!(setup_session).set_on_start())
            .register_transaction(transaction!(tx_sync_get_child_version)),
    );
    if profile.include_sync_mixed() {
        attack = attack.register_scenario(
            scenario!("SyncMixed")
                .set_weight(3)?
                .register_transaction(transaction!(setup_session).set_on_start())
                .register_transaction(transaction!(tx_sync_add_version))
                .register_transaction(transaction!(tx_sync_get_child_version))
                .register_transaction(transaction!(tx_sync_add_version))
                .register_transaction(transaction!(tx_sync_snapshot)),
        );
    }
    if profile.include_bridge_sync() {
        attack = attack.register_scenario(
            scenario!("BridgeSync")
                .set_weight(match profile {
                    LoadProfile::MultiDeviceSingleUser => 1,
                    LoadProfile::TeamContention => 3,
                    _ => 2,
                })?
                .register_transaction(transaction!(setup_session).set_on_start())
                .register_transaction(transaction!(tx_bridge_push_then_read)),
        );
    }
    attack.execute().await?;

    Ok(())
}

// --- Setup ---

async fn setup_session(user: &mut GooseUser) -> TransactionResult {
    let tokens = LOADED_TOKENS.get().expect("tokens not loaded");
    let profile = *LOAD_PROFILE.get().expect("profile not loaded");

    let vu_index = USER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let personal: Vec<TokenEntry> = tokens
        .iter()
        .filter(|t| t.user_type == "personal")
        .cloned()
        .collect();
    let team = tokens.iter().find(|t| t.user_type == "team").cloned();
    let multi_device: Vec<TokenEntry> = tokens
        .iter()
        .filter(|t| t.user_type == "multi_device")
        .cloned()
        .collect();

    let entry = match profile {
        LoadProfile::Mixed => {
            if vu_index < personal.len() {
                personal[vu_index].clone()
            } else {
                team.unwrap_or_else(|| tokens[vu_index % tokens.len()].clone())
            }
        }
        LoadProfile::PersonalOnly => personal
            .get(vu_index % personal.len().max(1))
            .cloned()
            .unwrap_or_else(|| tokens[vu_index % tokens.len()].clone()),
        LoadProfile::TeamContention => {
            team.unwrap_or_else(|| tokens[vu_index % tokens.len()].clone())
        }
        LoadProfile::MultiDeviceSingleUser => multi_device
            .get(vu_index % multi_device.len().max(1))
            .cloned()
            .unwrap_or_else(|| tokens[vu_index % tokens.len()].clone()),
    };

    user.set_session_data(UserSession {
        token: entry.bearer_token,
        user_type: entry.user_type,
        last_created_uuid: None,
        sync_client_id: entry.sync_client_id,
        sync_secret: entry.sync_secret,
        sync_latest_version: None,
    });

    Ok(())
}

fn get_token(user: &GooseUser) -> String {
    let session = user.get_session_data::<UserSession>().unwrap_or_else(|| {
        panic!("load-test session missing UserSession; setup_session did not run")
    });
    assert!(
        !session.token.is_empty(),
        "load-test session has an empty bearer token"
    );
    session.token.clone()
}

// --- Request helpers ---

async fn auth_get(
    user: &mut GooseUser,
    path: &str,
) -> Result<GooseResponse, Box<TransactionError>> {
    let token = get_token(user);
    let builder = user.get_request_builder(&GooseMethod::Get, path)?;
    let req = GooseRequest::builder()
        .method(GooseMethod::Get)
        .path(path)
        .expect_status_code(200)
        .set_request_builder(
            builder
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(10)),
        )
        .build();
    user.request(req).await
}

async fn auth_post<T: serde::Serialize>(
    user: &mut GooseUser,
    path: &str,
    body: &T,
) -> Result<GooseResponse, Box<TransactionError>> {
    let token = get_token(user);
    let builder = user.get_request_builder(&GooseMethod::Post, path)?;
    let req = GooseRequest::builder()
        .method(GooseMethod::Post)
        .path(path)
        .expect_status_code(200)
        .set_request_builder(
            builder
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(10))
                .json(body),
        )
        .build();
    user.request(req).await
}

/// Extract UUID from "Created task <uuid>." response output.
fn extract_uuid(body: &serde_json::Value) -> Option<String> {
    body["output"]
        .as_str()
        .and_then(|s| s.strip_prefix("Created task "))
        .and_then(|s| s.strip_suffix('.'))
        .map(|s| s.to_string())
}

/// Parse response body as JSON and extract UUID.
/// Returns None and logs a warning on parse or extraction failure.
async fn parse_create_response(response: reqwest::Response) -> Option<String> {
    let body: serde_json::Value = match response.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Failed to parse create response JSON: {e}");
            return None;
        }
    };

    let uuid = extract_uuid(&body);
    if uuid.is_none() {
        tracing::warn!("UUID not found in response: {}", body["output"]);
    }
    uuid
}

// --- Read transactions ---

async fn tx_list_tasks(user: &mut GooseUser) -> TransactionResult {
    let _ = auth_get(user, "/api/tasks").await?;
    Ok(())
}

async fn tx_get_views(user: &mut GooseUser) -> TransactionResult {
    let _ = auth_get(user, "/api/views").await?;
    Ok(())
}

async fn tx_get_app_config(user: &mut GooseUser) -> TransactionResult {
    let _ = auth_get(user, "/api/app-config").await?;
    Ok(())
}

async fn tx_healthz(user: &mut GooseUser) -> TransactionResult {
    let builder = user.get_request_builder(&GooseMethod::Get, "/healthz")?;
    let req = GooseRequest::builder()
        .path("/healthz")
        .expect_status_code(200)
        .set_request_builder(builder.timeout(std::time::Duration::from_secs(10)))
        .build();
    let _ = user.request(req).await?;
    Ok(())
}

// --- Write transactions ---

async fn tx_add_complete_delete(user: &mut GooseUser) -> TransactionResult {
    let add_body = serde_json::json!({"raw": "+load_test priority:M Load test task"});
    let goose = auth_post(user, "/api/tasks", &add_body).await?;

    // expect_status_code already marks the request as failed in Goose metrics.
    // We still need to extract the response to continue the lifecycle.
    let response = match goose.response {
        Ok(r) if r.status().is_success() => r,
        _ => return Ok(()), // Can't continue lifecycle without a UUID
    };

    let uuid = match parse_create_response(response).await {
        Some(u) => u,
        None => return Ok(()),
    };

    let done_path = format!("/api/tasks/{uuid}/done");
    let _ = auth_post(user, &done_path, &serde_json::json!({})).await?;

    let del_path = format!("/api/tasks/{uuid}/delete");
    let _ = auth_post(user, &del_path, &serde_json::json!({})).await?;

    Ok(())
}

async fn tx_add_task(user: &mut GooseUser) -> TransactionResult {
    let add_body =
        serde_json::json!({"raw": "+load_test project:LOADTEST.Mixed Mixed load test task"});
    let goose = auth_post(user, "/api/tasks", &add_body).await?;

    // Request failure already recorded by expect_status_code
    let response = match goose.response {
        Ok(r) if r.status().is_success() => r,
        _ => return Ok(()), // Can't store UUID without a successful response
    };

    let uuid = match parse_create_response(response).await {
        Some(u) => u,
        None => return Ok(()),
    };
    if let Some(session) = user.get_session_data_mut::<UserSession>() {
        session.last_created_uuid = Some(uuid);
    }

    Ok(())
}

/// Delete the most recently created task (cleanup for Mixed scenario).
///
/// Only clears the UUID on successful delete (200) or task already gone (404).
/// On transient failure (503), the UUID is preserved so the next iteration
/// can retry rather than leaving an orphan. 404 is marked as success in Goose
/// since the task being gone is the desired outcome.
async fn tx_delete_latest(user: &mut GooseUser) -> TransactionResult {
    let uuid = user
        .get_session_data::<UserSession>()
        .and_then(|s| s.last_created_uuid.clone());

    let uuid = match uuid {
        Some(u) => u,
        None => return Ok(()),
    };

    let token = get_token(user);
    let path = format!("/api/tasks/{uuid}/delete");
    let builder = user.get_request_builder(&GooseMethod::Post, &path)?;
    let req = GooseRequest::builder()
        .method(GooseMethod::Post)
        .path(&*path)
        .set_request_builder(
            builder
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(10))
                .json(&serde_json::json!({})),
        )
        .build();

    let mut goose = user.request(req).await?;

    if let Ok(resp) = &goose.response {
        let status = resp.status().as_u16();
        if status == 200 || status == 404 {
            // Task deleted or already gone — clear the UUID
            if let Some(session) = user.get_session_data_mut::<UserSession>() {
                session.last_created_uuid = None;
            }
            if status == 404 {
                return user.set_success(&mut goose.request);
            }
        }
        // On 503/other: keep UUID for retry in next iteration
    }

    Ok(())
}

/// Modify the most recently created task.
///
/// Accepts 404 as a valid response under contention — the task may have been
/// deleted by a concurrent sync bridge operation or a racing WriteHeavy VU on
/// the shared team replica. On 404, clears the stale UUID from the session.
async fn tx_modify_latest(user: &mut GooseUser) -> TransactionResult {
    let uuid = user
        .get_session_data::<UserSession>()
        .and_then(|s| s.last_created_uuid.clone());

    let uuid = match uuid {
        Some(u) => u,
        None => return Ok(()),
    };

    let modify_body = serde_json::json!({
        "priority": "H",
        "description": "Modified by load test"
    });

    let token = get_token(user);
    let path = format!("/api/tasks/{uuid}/modify");
    let builder = user.get_request_builder(&GooseMethod::Post, &path)?;
    let req = GooseRequest::builder()
        .method(GooseMethod::Post)
        .path(&*path)
        .set_request_builder(
            builder
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(10))
                .json(&modify_body),
        )
        .build();

    let mut goose = user.request(req).await?;

    if let Ok(resp) = &goose.response {
        let status = resp.status().as_u16();
        if status == 404 || status == 409 {
            // Task was deleted/completed by concurrent operation — clear stale UUID
            // and mark as success (not a server error, just contention)
            if let Some(session) = user.get_session_data_mut::<UserSession>() {
                session.last_created_uuid = None;
            }
            return user.set_success(&mut goose.request);
        }
    }

    Ok(())
}

// --- Contention transactions ---
// Multiple VUs concurrently modify the same task to test write-write conflicts.

/// Ensure the shared "hot task" exists. Called every iteration (not just on_start)
/// so that if the hot task is deleted by sync conflicts, it gets recreated.
/// Creates the task WITHOUT holding the write lock across network I/O, then
/// acquires the lock only to store the UUID.
async fn tx_contention_ensure_hot_task(user: &mut GooseUser) -> TransactionResult {
    let lock = HOT_TASK_UUID.get().unwrap();

    // Fast path: read lock check
    {
        let read = lock.read().await;
        if read.is_some() {
            return Ok(());
        }
    }

    // Create the task without holding any lock (avoids blocking other VUs during I/O)
    let add_body =
        serde_json::json!({"raw": "+load_test +hot_task project:CONTENTION Hot contention task"});
    let goose = auth_post(user, "/api/tasks", &add_body).await?;

    let response = match goose.response {
        Ok(r) if r.status().is_success() => r,
        _ => return Ok(()),
    };

    let uuid = match parse_create_response(response).await {
        Some(u) => u,
        None => return Ok(()),
    };

    // Now acquire write lock briefly to store the UUID (double-check inside)
    let mut write = lock.write().await;
    if write.is_none() {
        *write = Some(uuid);
    }
    // If another VU already set it, our created task is orphaned — acceptable
    // for a load test (extra task doesn't affect results)

    Ok(())
}

/// Modify the shared hot task — many VUs do this concurrently to force contention.
///
/// If the hot task returns 404 (deleted by sync bridge conflict resolution under
/// extreme contention), resets HOT_TASK_UUID so `tx_contention_setup` recreates
/// it on the next VU that checks. Marks 404/409 as success in Goose — these
/// are expected protocol responses, not server errors.
async fn tx_contention_modify(user: &mut GooseUser) -> TransactionResult {
    let lock = HOT_TASK_UUID.get().unwrap();
    let uuid = {
        let read = lock.read().await;
        match read.as_ref() {
            Some(u) => u.clone(),
            None => return Ok(()),
        }
    };

    // Increment per-call counter so each modify has different data (true contention)
    let counter = CONTENTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let priority = match counter % 3 {
        0 => "H",
        1 => "M",
        _ => "L",
    };

    let modify_body = serde_json::json!({
        "priority": priority,
        "description": format!("Contention test iteration {counter}")
    });

    let token = get_token(user);
    let path = format!("/api/tasks/{uuid}/modify");
    let builder = user.get_request_builder(&GooseMethod::Post, &path)?;
    let req = GooseRequest::builder()
        .method(GooseMethod::Post)
        .path(&*path)
        .set_request_builder(
            builder
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(10))
                .json(&modify_body),
        )
        .build();

    let mut goose = user.request(req).await?;

    if let Ok(resp) = &goose.response {
        let status = resp.status().as_u16();
        if status == 404 {
            // Hot task was deleted (sync conflict under extreme contention).
            // Reset so tx_contention_setup recreates it.
            let mut write = lock.write().await;
            *write = None;
            return user.set_success(&mut goose.request);
        }
        if status == 409 {
            // Task was concurrently modified to completed/deleted — valid contention
            return user.set_success(&mut goose.request);
        }
    }

    Ok(())
}

// --- Sync protocol transactions ---

fn get_sync_client_id(user: &GooseUser) -> Option<String> {
    user.get_session_data::<UserSession>()
        .and_then(|s| s.sync_client_id.clone())
}

fn get_sync_secret(user: &GooseUser) -> Option<String> {
    user.get_session_data::<UserSession>()
        .and_then(|s| s.sync_secret.clone())
}

fn get_sync_latest_version(user: &GooseUser) -> String {
    user.get_session_data::<UserSession>()
        .and_then(|s| s.sync_latest_version.clone())
        .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".to_string())
}

fn build_encrypted_history_segment(
    client_id: &str,
    sync_secret: &str,
    parent: &str,
) -> Option<Vec<u8>> {
    let client_uuid = Uuid::parse_str(client_id).ok()?;
    let parent_uuid = Uuid::parse_str(parent).ok()?;
    let cryptor = SyncCryptor::new(client_uuid, sync_secret.as_bytes()).ok()?;
    let task_uuid = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
    let version_json = serde_json::json!({
        "operations": [
            { "Create": { "uuid": task_uuid.to_string() } },
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "status",
                "value": "pending",
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "description",
                "value": format!("load-test sync task {}", rand::random::<u64>()),
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "entry",
                "value": now,
                "timestamp": now
            }},
            { "Update": {
                "uuid": task_uuid.to_string(),
                "property": "modified",
                "value": now,
                "timestamp": now
            }}
        ]
    });
    let plaintext = serde_json::to_vec(&version_json).ok()?;
    cryptor.seal(parent_uuid, &plaintext).ok()
}

/// POST /v1/client/add-version/{parent} — push a new version
async fn tx_sync_add_version(user: &mut GooseUser) -> TransactionResult {
    let client_id = match get_sync_client_id(user) {
        Some(id) => id,
        None => return Ok(()),
    };
    let sync_secret = match get_sync_secret(user) {
        Some(secret) => secret,
        None => return Ok(()),
    };
    let parent = get_sync_latest_version(user);

    let path = format!("/v1/client/add-version/{parent}");
    let builder = user.get_request_builder(&GooseMethod::Post, &path)?;
    let payload = match build_encrypted_history_segment(&client_id, &sync_secret, &parent) {
        Some(payload) => payload,
        None => return Ok(()),
    };

    let req = GooseRequest::builder()
        .method(GooseMethod::Post)
        .name("POST /v1/client/add-version/{parent}")
        .path(&*path)
        .set_request_builder(
            builder
                .header("X-Client-Id", &client_id)
                .header(
                    "Content-Type",
                    "application/vnd.taskchampion.history-segment",
                )
                .body(payload)
                .timeout(std::time::Duration::from_secs(10)),
        )
        .build();

    let mut goose = user.request(req).await?;

    if let Ok(resp) = &goose.response {
        let status = resp.status();
        if status.is_success() {
            // Success — update latest version from response header
            if let Some(vid) = resp.headers().get("X-Version-Id") {
                if let Ok(vid_str) = vid.to_str() {
                    if let Some(session) = user.get_session_data_mut::<UserSession>() {
                        session.sync_latest_version = Some(vid_str.to_string());
                    }
                }
            }
        } else if status.as_u16() == 409 {
            // Conflict — expected on shared replicas. Update parent from response
            // so next attempt uses the correct parent (converges instead of spamming).
            if let Some(pid) = resp.headers().get("X-Parent-Version-Id") {
                if let Ok(pid_str) = pid.to_str() {
                    if let Some(session) = user.get_session_data_mut::<UserSession>() {
                        session.sync_latest_version = Some(pid_str.to_string());
                    }
                }
            }
            // Mark as success in Goose metrics — 409 is a valid protocol response
            return user.set_success(&mut goose.request);
        }
    }

    Ok(())
}

/// GET /v1/client/get-child-version/{parent} — pull next version
///
/// Uses the VU's tracked sync_latest_version as the parent, simulating
/// a real `task sync` pull that traverses the chain incrementally.
/// 404 = up to date (no child), 410 = parent unknown (sync error).
/// Both are valid protocol responses, not failures.
async fn tx_sync_get_child_version(user: &mut GooseUser) -> TransactionResult {
    let client_id = match get_sync_client_id(user) {
        Some(id) => id,
        None => return Ok(()),
    };

    // Use the VU's latest known version as parent (incremental traversal)
    let parent = get_sync_latest_version(user);
    let path = format!("/v1/client/get-child-version/{parent}");
    let builder = user.get_request_builder(&GooseMethod::Get, &path)?;

    let req = GooseRequest::builder()
        .method(GooseMethod::Get)
        .name("GET /v1/client/get-child-version/{parent}")
        .path(&*path)
        .set_request_builder(
            builder
                .header("X-Client-Id", &client_id)
                .timeout(std::time::Duration::from_secs(10)),
        )
        .build();

    let mut goose = user.request(req).await?;

    if let Ok(resp) = &goose.response {
        let status = resp.status().as_u16();
        if status == 200 {
            // Advance the VU's chain pointer so next read fetches the next version
            if let Some(vid) = resp.headers().get("X-Version-Id") {
                if let Ok(vid_str) = vid.to_str() {
                    if let Some(session) = user.get_session_data_mut::<UserSession>() {
                        session.sync_latest_version = Some(vid_str.to_string());
                    }
                }
            }
        } else if status == 404 || status == 410 {
            // 404 = up to date, 410 = parent unknown — both valid
            return user.set_success(&mut goose.request);
        }
    }

    Ok(())
}

/// POST /v1/client/add-snapshot + GET /v1/client/snapshot
async fn tx_sync_snapshot(user: &mut GooseUser) -> TransactionResult {
    let client_id = match get_sync_client_id(user) {
        Some(id) => id,
        None => return Ok(()),
    };

    let builder = user.get_request_builder(&GooseMethod::Get, "/v1/client/snapshot")?;
    let req = GooseRequest::builder()
        .method(GooseMethod::Get)
        .name("GET /v1/client/snapshot")
        .path("/v1/client/snapshot")
        .set_request_builder(
            builder
                .header("X-Client-Id", &client_id)
                .timeout(std::time::Duration::from_secs(10)),
        )
        .build();

    let _ = user.request(req).await?;

    Ok(())
}

// --- Bridge sync transaction ---
// Tests the real-world cross-protocol flow: TW CLI pushes a version via TC sync,
// then iOS app reads tasks via REST API. In the queued bridge model the REST
// read sees canonical state while a background bridge worker reconciles device
// chains; this scenario measures the cost of that cross-protocol pressure.

/// Push a version via TC sync, then immediately read tasks via REST.
///
/// Measures the latency overhead of the queued sync bridge path under load:
/// TC pushes still reconcile the canonical replica directly, while REST reads
/// and writes enqueue background bridge work rather than blocking inline.
///
/// This is a **performance** test, not a propagation correctness test.
/// TC sync payloads are opaque blobs — the server stores them without
/// interpretation. Data propagation correctness is verified by the
/// sync_bridge_integration.rs test suite.
///
/// Without CMDOCK_MASTER_KEY, the bridge is a no-op and this is just a
/// TC push + REST read (still useful for measuring protocol overhead).
async fn tx_bridge_push_then_read(user: &mut GooseUser) -> TransactionResult {
    let client_id = match get_sync_client_id(user) {
        Some(id) => id,
        None => return Ok(()),
    };
    let sync_secret = match get_sync_secret(user) {
        Some(secret) => secret,
        None => return Ok(()),
    };
    let token = get_token(user);
    let parent = get_sync_latest_version(user);

    // Step 1: Push a version via TC sync protocol (simulates `task sync` from TW CLI)
    let push_path = format!("/v1/client/add-version/{parent}");
    let builder = user.get_request_builder(&GooseMethod::Post, &push_path)?;
    let payload = match build_encrypted_history_segment(&client_id, &sync_secret, &parent) {
        Some(payload) => payload,
        None => return Ok(()),
    };

    let req = GooseRequest::builder()
        .method(GooseMethod::Post)
        .name("BridgeSync: POST /v1/client/add-version")
        .path(&*push_path)
        .set_request_builder(
            builder
                .header("X-Client-Id", &client_id)
                .header(
                    "Content-Type",
                    "application/vnd.taskchampion.history-segment",
                )
                .body(payload)
                .timeout(std::time::Duration::from_secs(10)),
        )
        .build();

    let mut goose = user.request(req).await?;

    // Update version tracking on success or conflict
    if let Ok(resp) = &goose.response {
        let status = resp.status();
        if status.is_success() {
            if let Some(vid) = resp.headers().get("X-Version-Id") {
                if let Ok(vid_str) = vid.to_str() {
                    if let Some(session) = user.get_session_data_mut::<UserSession>() {
                        session.sync_latest_version = Some(vid_str.to_string());
                    }
                }
            }
        } else if status.as_u16() == 409 {
            if let Some(pid) = resp.headers().get("X-Parent-Version-Id") {
                if let Ok(pid_str) = pid.to_str() {
                    if let Some(session) = user.get_session_data_mut::<UserSession>() {
                        session.sync_latest_version = Some(pid_str.to_string());
                    }
                }
            }
            // 409 is valid — mark as success
            let _ = user.set_success(&mut goose.request);
        }
    }

    // Step 2: Read tasks via REST API (triggers sync bridge pull if master_key is set)
    let builder = user.get_request_builder(&GooseMethod::Get, "/api/tasks")?;
    let req = GooseRequest::builder()
        .method(GooseMethod::Get)
        .name("BridgeSync: GET /api/tasks")
        .path("/api/tasks")
        .expect_status_code(200)
        .set_request_builder(
            builder
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(15)),
        )
        .build();

    let _ = user.request(req).await?;

    Ok(())
}
