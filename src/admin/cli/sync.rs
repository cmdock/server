use std::path::Path;

use crate::admin::cli::SyncAction;
use crate::admin::services::sync_identity::SyncIdentityService;

use super::common::{open_store, require_master_key};

pub(super) async fn run(action: SyncAction, data_dir: &Path) -> anyhow::Result<()> {
    let store = open_store(data_dir).await?;
    let sync_identity = SyncIdentityService::new(store.clone());

    match action {
        SyncAction::Create { user_id } => {
            if store.get_user_by_id(&user_id).await?.is_none() {
                anyhow::bail!("User not found: {user_id}");
            }

            let created = sync_identity
                .create_for_user(&user_id, require_master_key()?)
                .await?;

            tracing::info!(
                target: "audit",
                action = "replica.create",
                source = "cli",
                client_ip = "local",
                user_id = %user_id,
                client_id = %created.client_id,
            );

            println!("Canonical sync identity created:");
            println!("  User:               {user_id}");
            println!("  Canonical Client ID:{}", created.client_id);
            println!();
            println!("This is the user's server-side sync identity.");
            println!("Register individual devices with:");
            println!("  cmdock-server admin device create {user_id} --name <device-name>");
        }

        SyncAction::Show { user_id } => {
            if store.get_user_by_id(&user_id).await?.is_none() {
                anyhow::bail!("User not found: {user_id}");
            }

            match sync_identity.get_for_user(&user_id).await? {
                Some(replica) => {
                    println!("Canonical sync identity for user {user_id}:");
                    println!("  Client ID: {}", replica.id);
                    println!("  Label:     {}", replica.label);
                    println!("  Created:   {}", replica.created_at);
                    println!();
                    println!(
                        "This is the server-side canonical identity, not a device credential set."
                    );
                    println!("Register individual devices with:");
                    println!("  cmdock-server admin device create {user_id} --name <device-name>");
                }
                None => {
                    println!("No canonical sync identity for user {user_id}.");
                    println!("Create one with:");
                    println!("  cmdock-server admin sync create {user_id}");
                }
            }
        }

        SyncAction::ShowSecret { user_id } => {
            if store.get_user_by_id(&user_id).await?.is_none() {
                anyhow::bail!("User not found: {user_id}");
            }

            let replica = sync_identity
                .decrypt_secret_for_user(&user_id, require_master_key()?)
                .await?;

            println!("Encryption secret for user {user_id}:");
            println!("  Client ID:          {}", replica.client_id);
            println!("  Encryption Secret:  {}", replica.encryption_secret_hex);
            println!();
            println!("Canonical secret shown for debug/migration use.");
            println!("Routine device onboarding should use:");
            println!("  cmdock-server admin device create {user_id} --name <device-name>");
        }

        SyncAction::Delete { user_id } => {
            if store.get_user_by_id(&user_id).await?.is_none() {
                anyhow::bail!("User not found: {user_id}");
            }

            let deleted = sync_identity.delete_for_user(&user_id).await?;
            if deleted {
                tracing::info!(
                    target: "audit",
                    action = "replica.delete",
                    source = "cli",
                    client_ip = "local",
                    user_id = %user_id,
                );
                println!("Replica deleted for user {user_id}.");
            } else {
                println!("No replica found for user {user_id}.");
            }
        }
    }

    let _ = data_dir;
    Ok(())
}
