use std::path::Path;

use chrono::{Duration, Utc};

use crate::admin::cli::ConnectConfigAction;
use crate::config::ServerConfig;
use crate::connect_config::{
    build_connect_url, normalize_connect_server_url, render_terminal_qr,
    DEFAULT_CONNECT_TOKEN_BYTES, DEFAULT_CONNECT_TOKEN_TTL_MINUTES,
};

use super::common::open_store;

pub(super) async fn run(
    action: ConnectConfigAction,
    data_dir: &Path,
    config: Option<&ServerConfig>,
) -> anyhow::Result<()> {
    let store = open_store(data_dir).await?;

    match action {
        ConnectConfigAction::Create {
            user_id,
            server_url,
            name,
            expires_minutes,
            no_qr,
        } => {
            let user = store
                .get_user_by_id(&user_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("User not found: {user_id}"))?;
            let server_url = resolve_connect_server_url(config, server_url.as_deref())?;
            let expires_minutes = if expires_minutes == 0 {
                DEFAULT_CONNECT_TOKEN_TTL_MINUTES as u32
            } else {
                expires_minutes
            };
            let expires_at = (Utc::now() + Duration::minutes(i64::from(expires_minutes)))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            let issued = store
                .create_connect_config_token(&user.id, &expires_at, DEFAULT_CONNECT_TOKEN_BYTES)
                .await?;
            let connect_url = build_connect_url(
                &server_url,
                Some(&issued.token_id),
                name.as_deref(),
                &issued.token,
            )?;

            tracing::info!(
                target: "boundary",
                event = "connect_config.token_issued",
                component = "cmdock/server",
                correlation_id = %issued.token_id,
                credential_hash_prefix = %issued.credential_hash_prefix,
                source = "cli",
                user_id = %user.id,
                expires_at = %issued.expires_at,
                request_id = ?Option::<String>::None,
            );

            tracing::info!(
                target: "audit",
                action = "connect_config.generate",
                source = "cli",
                client_ip = "local",
                user_id = %user.id,
                token_id = %issued.token_id,
                credential_hash_prefix = %issued.credential_hash_prefix,
                expires_at = %expires_at,
                url_length = connect_url.len(),
            );

            println!("Connect config generated:");
            println!("  User:        {} ({})", user.username, user.id);
            println!("  Server URL:  {server_url}");
            println!("  Expires At:  {expires_at} UTC");
            println!("  URL Length:  {} bytes", connect_url.len());
            println!("  Token ID:    {}", issued.token_id);
            println!("  Token Hash:  {}", issued.credential_hash_prefix);
            if let Some(name) = name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                println!("  Name:        {name}");
            }
            println!();
            println!("{connect_url}");

            if !no_qr {
                println!();
                println!("Scan QR:");
                println!("{}", render_terminal_qr(&connect_url)?);
            }

            println!("The embedded credential is short-lived and cannot be retrieved later.");
        }
    }

    Ok(())
}

fn resolve_connect_server_url(
    config: Option<&ServerConfig>,
    override_url: Option<&str>,
) -> anyhow::Result<String> {
    match override_url {
        Some(url) => normalize_connect_server_url(url),
        None => {
            let raw = config
                .and_then(|cfg| cfg.server.public_base_url.as_deref())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "connect-config server URL is not configured; set [server].public_base_url or pass --server-url"
                    )
                })?;
            normalize_connect_server_url(raw)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdminSection, AuditSection, ServerConfig, ServerSection};
    use std::path::PathBuf;

    fn config_with_public_base_url(url: Option<&str>) -> ServerConfig {
        ServerConfig {
            server: ServerSection {
                host: "127.0.0.1".to_string(),
                port: 8080,
                data_dir: PathBuf::from("./data"),
                public_base_url: url.map(|value| value.to_string()),
                trust_forwarded_headers: false,
            },
            admin: AdminSection::default(),
            backup_dir: None,
            backup_retention_count: 7,
            llm: None,
            audit: AuditSection::default(),
            master_key: None,
        }
    }

    #[test]
    fn test_resolve_connect_server_url_prefers_explicit_override() {
        let config = config_with_public_base_url(Some("https://tasks.example.com"));
        let resolved =
            resolve_connect_server_url(Some(&config), Some("https://override.example.com"))
                .unwrap();
        assert_eq!(resolved, "https://override.example.com");
    }

    #[test]
    fn test_resolve_connect_server_url_uses_config_default() {
        let config = config_with_public_base_url(Some("https://tasks.example.com/"));
        let resolved = resolve_connect_server_url(Some(&config), None).unwrap();
        assert_eq!(resolved, "https://tasks.example.com");
    }

    #[test]
    fn test_resolve_connect_server_url_requires_https_origin() {
        let config = config_with_public_base_url(Some("http://tasks.example.com"));
        let err = resolve_connect_server_url(Some(&config), None).unwrap_err();
        assert!(err.to_string().contains("https://"));
    }
}
