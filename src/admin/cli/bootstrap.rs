use std::path::Path;

use serde::Serialize;

use crate::admin::cli::BootstrapAction;
use crate::admin::services::bootstrap::{BootstrapService, BootstrapUserDeviceRequest};

use super::common::{open_store, render_taskrc_lines, require_master_key};

#[derive(Debug, Serialize)]
struct BootstrapUserDeviceJson {
    user_id: String,
    username: String,
    canonical_client_id: String,
    client_id: String,
    device_client_id: String,
    encryption_secret: String,
    server_url: String,
    bootstrap_status: String,
    created_user: bool,
    bootstrap_request_id: String,
}

pub(super) async fn run(action: BootstrapAction, data_dir: &Path) -> anyhow::Result<()> {
    let store = open_store(data_dir).await?;

    match action {
        BootstrapAction::UserDevice {
            user_id,
            username,
            create_user_if_missing,
            device_name,
            bootstrap_request_id,
            server_url,
            json,
        } => {
            let master_key = require_master_key()?;
            let service = BootstrapService::new(store, data_dir.to_path_buf());
            let result = service
                .bootstrap_user_device(
                    BootstrapUserDeviceRequest {
                        user_id,
                        username,
                        create_user_if_missing,
                        device_name,
                        bootstrap_request_id: bootstrap_request_id.clone(),
                    },
                    Some(master_key),
                )
                .await?;

            tracing::info!(
                target: "audit",
                action = "admin.bootstrap.user_device",
                source = "cli",
                client_ip = "local",
                user_id = %result.user.id,
                username = %result.user.username,
                device_client_id = %result.device_client_id,
                canonical_client_id = %result.canonical_client_id,
                bootstrap_status = %result.bootstrap_status,
                created_user = result.created_user,
            );

            if json {
                let payload = BootstrapUserDeviceJson {
                    user_id: result.user.id,
                    username: result.user.username,
                    canonical_client_id: result.canonical_client_id,
                    client_id: result.device_client_id.clone(),
                    device_client_id: result.device_client_id,
                    encryption_secret: result.encryption_secret_hex,
                    server_url,
                    bootstrap_status: result.bootstrap_status,
                    created_user: result.created_user,
                    bootstrap_request_id,
                };
                println!("{}", serde_json::to_string(&payload)?);
                return Ok(());
            }

            let lines = render_taskrc_lines(
                Some(&server_url),
                &result.device_client_id,
                &result.encryption_secret_hex,
            );
            println!("Bootstrap user/device credentials created or replayed:");
            println!("  User:                 {}", result.user.id);
            println!("  Username:             {}", result.user.username);
            println!("  Canonical Client ID:  {}", result.canonical_client_id);
            println!("  Device Client ID:     {}", result.device_client_id);
            println!("  Encryption Secret:    {}", result.encryption_secret_hex);
            println!("  Server URL:           {server_url}");
            println!("  Bootstrap Status:     {}", result.bootstrap_status);
            println!("  Created User:         {}", result.created_user);
            println!("  Bootstrap Request ID: {bootstrap_request_id}");
            println!();
            println!(
                "WARNING: these credentials are sensitive. Store them in your secrets manager."
            );
            println!();
            println!("Taskwarrior (.taskrc) snippet:");
            for line in &lines {
                println!("  {line}");
            }
        }
    }

    Ok(())
}
