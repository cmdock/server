use std::process::Command;
use std::sync::Arc;

use tempfile::TempDir;

use cmdock_server::crypto;
use cmdock_server::runtime_policy::{RuntimeAccessMode, RuntimeDeleteAction, RuntimePolicy};
use cmdock_server::store::models::NewUser;
use cmdock_server::store::sqlite::SqliteConfigStore;
use cmdock_server::store::ConfigStore;

fn admin_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cmdock-server")
}

fn master_key_bytes() -> [u8; 32] {
    [42u8; 32]
}

fn master_key_hex() -> String {
    "2a".repeat(32)
}

async fn setup_seeded_user() -> (TempDir, Arc<dyn ConfigStore>, String) {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("users")).unwrap();

    let db_path = tmp.path().join("config.sqlite");
    let store: Arc<dyn ConfigStore> = Arc::new(
        SqliteConfigStore::new(&db_path.to_string_lossy())
            .await
            .unwrap(),
    );
    store.run_migrations().await.unwrap();

    let user = store
        .create_user(&NewUser {
            username: "cli-device-user".to_string(),
            password_hash: String::new(),
        })
        .await
        .unwrap();

    std::fs::create_dir_all(tmp.path().join("users").join(&user.id)).unwrap();

    let raw_secret = b"cli-test-master-encryption-secret";
    let encrypted = crypto::encrypt_secret(raw_secret, &master_key_bytes()).unwrap();
    let enc_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encrypted);
    let replica_client_id = uuid::Uuid::new_v4().to_string();
    store
        .create_replica(&user.id, &replica_client_id, &enc_b64)
        .await
        .unwrap();

    (tmp, store, user.id)
}

fn run_admin(data_dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(admin_bin())
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env("CMDOCK_MASTER_KEY", master_key_hex())
        .output()
        .unwrap()
}

fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn extract_field(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix).map(str::trim))
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn test_admin_device_create_and_taskrc() {
    let (tmp, _store, user_id) = setup_seeded_user().await;

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "create",
            &user_id,
            "--name",
            "CLI MacBook",
            "--server-url",
            "https://sync.example.com",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    assert!(stdout.contains("Device created:"));
    assert!(stdout.contains("CLI MacBook"));
    assert!(stdout.contains("Taskwarrior (.taskrc) snippet:"));
    assert!(stdout.contains("sync.server.url=https://sync.example.com"));
    assert!(stdout.contains("sync.server.client_id="));
    assert!(stdout.contains("sync.encryption_secret="));
    assert!(stdout.contains("Manual iOS setup uses the same values:"));

    let client_id = extract_field(&stdout, "Client ID:");
    let legacy_path = tmp
        .path()
        .join("users")
        .join(&user_id)
        .join("sync")
        .join(format!("{client_id}.sqlite"));
    assert!(
        !legacy_path.exists(),
        "normal CLI device creation should not create legacy per-device sync DBs"
    );

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "taskrc",
            &user_id,
            &client_id,
            "--server-url",
            "https://sync.example.com",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    assert!(stdout.contains("sync.server.url=https://sync.example.com"));
    assert!(stdout.contains(&format!("sync.server.client_id={client_id}")));
    assert!(stdout.contains("sync.encryption_secret="));
}

#[tokio::test]
async fn test_admin_device_revoke_unrevoke_delete() {
    let (tmp, store, user_id) = setup_seeded_user().await;

    let output = run_admin(
        tmp.path(),
        &["admin", "device", "create", &user_id, "--name", "CLI Phone"],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = stdout_string(&output);
    let client_id = extract_field(&stdout, "Client ID:");

    let output = run_admin(
        tmp.path(),
        &["admin", "device", "revoke", &user_id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");
    let device = store.get_device(&client_id).await.unwrap().unwrap();
    assert_eq!(device.status, "revoked");

    let output = run_admin(
        tmp.path(),
        &["admin", "device", "unrevoke", &user_id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");
    let device = store.get_device(&client_id).await.unwrap().unwrap();
    assert_eq!(device.status, "active");

    let output = run_admin(
        tmp.path(),
        &["admin", "device", "delete", &user_id, &client_id, "-y"],
    );
    assert!(
        !output.status.success(),
        "delete should refuse active devices: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Revoke it first"));

    let output = run_admin(
        tmp.path(),
        &["admin", "device", "revoke", &user_id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");

    let output = run_admin(
        tmp.path(),
        &["admin", "device", "delete", &user_id, &client_id, "-y"],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(store.get_device(&client_id).await.unwrap().is_none());
}

#[tokio::test]
async fn test_admin_device_create_rejected_by_runtime_policy() {
    let (tmp, store, user_id) = setup_seeded_user().await;
    let policy = RuntimePolicy {
        runtime_access: RuntimeAccessMode::Block,
        delete_action: RuntimeDeleteAction::Allow,
    };
    store
        .upsert_runtime_policy(
            &user_id,
            "policy-v1",
            &policy,
            Some("policy-v1"),
            Some(&policy),
            Some("2026-04-03 12:00:00"),
        )
        .await
        .unwrap();

    let output = run_admin(
        tmp.path(),
        &[
            "admin",
            "device",
            "create",
            &user_id,
            "--name",
            "Blocked CLI Device",
        ],
    );
    assert!(
        !output.status.success(),
        "device create should be rejected when runtime policy blocks access: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Runtime access blocked by policy"));
}
