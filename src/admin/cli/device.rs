use std::path::Path;

use crate::admin::cli::DeviceAction;
use crate::devices::service::{delete_owned_device, rename_owned_device, set_owned_device_revoked};

use super::common::{
    confirm, decrypt_device_secret, open_store, print_taskrc_block, render_taskrc_lines,
    require_master_key, require_user, resolve_device, taskrc_server_url,
};

pub(super) async fn run(action: DeviceAction, data_dir: &Path) -> anyhow::Result<()> {
    let store = open_store(data_dir).await?;

    match action {
        DeviceAction::List { user_id } => {
            require_user(&store, &user_id).await?;
            let devices = store.list_devices(&user_id).await?;
            if devices.is_empty() {
                println!("No devices found for user {user_id}.");
                println!("Register one with:");
                println!("  cmdock-server admin device create {user_id} --name <device-name>");
                return Ok(());
            }

            println!(
                "{:<12} {:<20} {:<10} {:<19} {:<19} LAST_SYNC_IP",
                "CLIENT_ID", "NAME", "STATUS", "REGISTERED", "LAST_SYNC_AT"
            );
            println!("{}", "-".repeat(104));
            for d in &devices {
                let cid = &d.client_id[..12.min(d.client_id.len())];
                let registered_at = &d.registered_at;
                let last_sync_at = d.last_sync_at.as_deref().unwrap_or("-");
                let last_sync_ip = d.last_sync_ip.as_deref().unwrap_or("-");
                println!(
                    "{:<12} {:<20} {:<10} {:<19} {:<19} {}",
                    cid, d.name, d.status, registered_at, last_sync_at, last_sync_ip
                );
            }
            println!("\n{} device(s)", devices.len());
        }

        DeviceAction::Create {
            user_id,
            name,
            server_url,
        } => {
            require_user(&store, &user_id).await?;
            let provisioned = crate::devices::service::provision_device(
                &*store,
                data_dir,
                &user_id,
                &name,
                Some(require_master_key()?),
            )
            .await?;

            tracing::info!(
                target: "audit",
                action = "device.register",
                source = "cli",
                client_ip = "local",
                user_id = %user_id,
                client_id = %provisioned.client_id,
                device_name = %provisioned.name,
            );

            let lines = render_taskrc_lines(
                server_url.as_deref(),
                &provisioned.client_id,
                &provisioned.encryption_secret_hex,
            );
            println!("Device created:");
            println!("  User:               {user_id}");
            println!("  Name:               {}", provisioned.name);
            println!("  Client ID:          {}", provisioned.client_id);
            println!(
                "  Encryption Secret:  {}",
                provisioned.encryption_secret_hex
            );
            println!();
            println!("Taskwarrior (.taskrc) snippet:");
            for line in &lines {
                println!("  {line}");
            }
            println!();
            println!("Manual iOS setup uses the same values:");
            println!(
                "  Server URL:         {}",
                taskrc_server_url(server_url.as_deref())
            );
            println!("  Client ID:          {}", provisioned.client_id);
            println!(
                "  Encryption Secret:  {}",
                provisioned.encryption_secret_hex
            );
            println!();
            println!("Reprint the snippet later with:");
            println!(
                "  cmdock-server admin device taskrc {user_id} {}",
                provisioned.client_id
            );
        }

        DeviceAction::Show { user_id, client_id } => {
            require_user(&store, &user_id).await?;
            let device = resolve_device(&store, &user_id, &client_id).await?;
            println!("Device for user {user_id}:");
            println!("  Client ID:     {}", device.client_id);
            println!("  Name:          {}", device.name);
            println!("  Status:        {}", device.status);
            println!("  Registered:    {}", device.registered_at);
            println!(
                "  Last Sync At:  {}",
                device.last_sync_at.as_deref().unwrap_or("-")
            );
            println!(
                "  Last Sync IP:  {}",
                device.last_sync_ip.as_deref().unwrap_or("-")
            );
            println!();
            println!("Reprint onboarding snippet:");
            println!(
                "  cmdock-server admin device taskrc {user_id} {}",
                device.client_id
            );
        }

        DeviceAction::Taskrc {
            user_id,
            client_id,
            server_url,
        } => {
            require_user(&store, &user_id).await?;
            let device = resolve_device(&store, &user_id, &client_id).await?;
            let master_key = require_master_key()?;
            let secret_hex = decrypt_device_secret(&device, &master_key)?;
            println!("# Device: {}", device.name);
            print_taskrc_block(server_url.as_deref(), &device.client_id, &secret_hex);
        }

        DeviceAction::Rename {
            user_id,
            client_id,
            name,
        } => {
            require_user(&store, &user_id).await?;
            let device = resolve_device(&store, &user_id, &client_id).await?;
            let name = rename_owned_device(&*store, &user_id, &device.client_id, &name).await?;
            println!("Renamed device {} to '{}'.", device.client_id, name);
        }

        DeviceAction::Revoke {
            user_id,
            client_id,
            yes,
        } => {
            require_user(&store, &user_id).await?;
            let device = resolve_device(&store, &user_id, &client_id).await?;
            if !yes
                && !confirm(&format!(
                    "Revoke device '{}' ({})? [y/N] ",
                    device.name, device.client_id
                ))?
            {
                println!("Cancelled.");
                return Ok(());
            }
            set_owned_device_revoked(&*store, &user_id, &device.client_id, true).await?;
            tracing::info!(
                target: "audit",
                action = "device.revoke",
                source = "cli",
                client_ip = "local",
                user_id = %user_id,
                client_id = %device.client_id,
            );
            println!("Device revoked: {} ({})", device.name, device.client_id);
            println!("Other devices are unaffected.");
        }

        DeviceAction::Unrevoke {
            user_id,
            client_id,
            yes,
        } => {
            require_user(&store, &user_id).await?;
            let device = resolve_device(&store, &user_id, &client_id).await?;
            if !yes
                && !confirm(&format!(
                    "Unrevoke device '{}' ({})? [y/N] ",
                    device.name, device.client_id
                ))?
            {
                println!("Cancelled.");
                return Ok(());
            }
            set_owned_device_revoked(&*store, &user_id, &device.client_id, false).await?;
            tracing::info!(
                target: "audit",
                action = "device.unrevoke",
                source = "cli",
                client_ip = "local",
                user_id = %user_id,
                client_id = %device.client_id,
            );
            println!("Device re-enabled: {} ({})", device.name, device.client_id);
            println!("The same device credentials are active again.");
        }

        DeviceAction::Delete {
            user_id,
            client_id,
            yes,
        } => {
            require_user(&store, &user_id).await?;
            let device = resolve_device(&store, &user_id, &client_id).await?;
            if device.status != "revoked" {
                anyhow::bail!(
                    "Refusing to delete active device {} ({}). Revoke it first, then delete it if you want to remove the record permanently.",
                    device.name,
                    device.client_id
                );
            }
            if !yes
                && !confirm(&format!(
                    "Delete device '{}' ({}) permanently? [y/N] ",
                    device.name, device.client_id
                ))?
            {
                println!("Cancelled.");
                return Ok(());
            }
            delete_owned_device(&*store, data_dir, &user_id, &device.client_id).await?;
            tracing::info!(
                target: "audit",
                action = "device.delete",
                source = "cli",
                client_ip = "local",
                user_id = %user_id,
                client_id = %device.client_id,
            );
            println!("Device deleted: {} ({})", device.name, device.client_id);
        }
    }

    Ok(())
}
