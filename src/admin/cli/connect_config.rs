use std::path::Path;

use crate::admin::cli::ConnectConfigAction;
use crate::admin::services::connect_config::{
    self as service, ConnectConfigService, IssueRequest, IssueSource,
};
use crate::config::ServerConfig;
use crate::connect_config::render_terminal_qr;

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
            scheme,
        } => {
            let user = store
                .get_user_by_id(&user_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("User not found: {user_id}"))?;
            let server_url = service::resolve_server_url(
                config.and_then(|cfg| cfg.server.public_base_url.as_deref()),
                server_url.as_deref(),
            )?;
            // Preserve existing CLI behaviour: `--expires-minutes 0` means
            // "use the service default", not "expire immediately".
            let ttl_minutes = if expires_minutes == 0 {
                None
            } else {
                Some(expires_minutes)
            };

            let svc = ConnectConfigService::new(store);
            let outcome = svc
                .issue(IssueRequest {
                    user_id: user.id.clone(),
                    display_name: name.clone(),
                    server_url,
                    ttl_minutes,
                    source: IssueSource::Cli {
                        scheme: scheme.clone(),
                    },
                })
                .await?;

            let connect_url = outcome
                .connect_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("CLI issuance returned no connect URL"))?;

            println!("Connect config generated:");
            println!("  User:        {} ({})", user.username, user.id);
            println!("  Server URL:  {}", outcome.server_url);
            println!("  Expires At:  {} UTC", outcome.expires_at);
            println!("  URL Length:  {} bytes", connect_url.len());
            println!("  Token ID:    {}", outcome.token_id);
            println!("  Token Hash:  {}", outcome.credential_hash_prefix);
            if let Some(name) = name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                println!("  Name:        {name}");
            }
            println!();
            println!("{connect_url}");

            if !no_qr {
                println!();
                println!("Scan QR:");
                println!("{}", render_terminal_qr(connect_url)?);
            }

            println!("The embedded credential is short-lived and cannot be retrieved later.");
        }
    }

    Ok(())
}
