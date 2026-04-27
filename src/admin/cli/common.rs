use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::crypto;
use crate::store::models::DeviceRecord;
use crate::store::sqlite::SqliteConfigStore;
use crate::store::ConfigStore;

pub(super) async fn open_store(data_dir: &Path) -> anyhow::Result<Arc<dyn ConfigStore>> {
    let db_path = data_dir.join("config.sqlite");
    let is_new = !db_path.exists();
    let store = SqliteConfigStore::new(&db_path.to_string_lossy()).await?;
    store.run_migrations().await?;
    if is_new {
        eprintln!("Initialised new database at {}", db_path.display());
    }
    Ok(Arc::new(store))
}

pub(super) fn require_master_key() -> anyhow::Result<[u8; 32]> {
    let raw = std::env::var("CMDOCK_MASTER_KEY").map_err(|_| {
        anyhow::anyhow!("CMDOCK_MASTER_KEY env var not set (required for replica management)")
    })?;
    crypto::parse_master_key(&raw)
}

pub(super) fn taskrc_server_url(server_url: Option<&str>) -> &str {
    server_url.unwrap_or("https://YOUR_SERVER")
}

pub(super) fn render_taskrc_lines(
    server_url: Option<&str>,
    client_id: &str,
    secret_hex: &str,
) -> [String; 3] {
    [
        format!("sync.server.url={}", taskrc_server_url(server_url)),
        format!("sync.server.client_id={client_id}"),
        format!("sync.encryption_secret={secret_hex}"),
    ]
}

pub(super) fn print_taskrc_block(server_url: Option<&str>, client_id: &str, secret_hex: &str) {
    for line in render_taskrc_lines(server_url, client_id, secret_hex) {
        println!("  {line}");
    }
}

pub(super) async fn require_user(
    store: &Arc<dyn ConfigStore>,
    user_id: &str,
) -> anyhow::Result<()> {
    if store.get_user_by_id(user_id).await?.is_none() {
        anyhow::bail!("User not found: {user_id}");
    }
    Ok(())
}

pub(super) async fn resolve_device(
    store: &Arc<dyn ConfigStore>,
    user_id: &str,
    client_id_or_prefix: &str,
) -> anyhow::Result<DeviceRecord> {
    let devices = store.list_devices(user_id).await?;

    let mut matches: Vec<DeviceRecord> = devices
        .into_iter()
        .filter(|d| {
            d.client_id == client_id_or_prefix || d.client_id.starts_with(client_id_or_prefix)
        })
        .collect();

    match matches.len() {
        0 => anyhow::bail!("Device not found for user {user_id}: {client_id_or_prefix}"),
        1 => Ok(matches.remove(0)),
        _ => anyhow::bail!(
            "Ambiguous device prefix '{client_id_or_prefix}' for user {user_id}. Use a longer prefix."
        ),
    }
}

pub(super) fn decrypt_device_secret(
    device: &DeviceRecord,
    master_key: &[u8; 32],
) -> anyhow::Result<String> {
    let secret_enc_b64 = device
        .encryption_secret_enc
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Device {} has no stored secret", device.client_id))?;
    let encrypted =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, secret_enc_b64)?;
    let secret_raw = crypto::decrypt_secret(&encrypted, master_key)?;
    Ok(hex::encode(secret_raw))
}

pub(super) fn confirm(prompt: &str) -> anyhow::Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_taskrc_lines_uses_placeholder_by_default() {
        let lines = render_taskrc_lines(None, "client-123", "secret-abc");
        assert_eq!(lines[0], "sync.server.url=https://YOUR_SERVER");
        assert_eq!(lines[1], "sync.server.client_id=client-123");
        assert_eq!(lines[2], "sync.encryption_secret=secret-abc");
    }

    #[test]
    fn test_render_taskrc_lines_uses_explicit_server_url() {
        let lines =
            render_taskrc_lines(Some("https://sync.example.com"), "client-123", "secret-abc");
        assert_eq!(lines[0], "sync.server.url=https://sync.example.com");
        assert_eq!(lines[1], "sync.server.client_id=client-123");
        assert_eq!(lines[2], "sync.encryption_secret=secret-abc");
    }
}
