use std::collections::HashSet;
use std::path::Path;

use crate::admin::cli::UserAction;
use crate::admin::prefix::{
    apply_prefix, derive_prefix, validate_prefix_format, SOURCE_OPERATOR, SOURCE_SIGNUP,
};
use crate::admin::services::recovery::RecoveryCoordinator;
use crate::runtime_policy::{runtime_delete_message, RuntimeDeleteDecision, RuntimePolicyService};
use crate::store::models::NewUser;
use uuid::Uuid;

use super::common::{confirm, open_store, require_user};

pub(super) async fn run(action: UserAction, data_dir: &Path) -> anyhow::Result<()> {
    let store = open_store(data_dir).await?;
    let recovery = RecoveryCoordinator::for_local(store.clone(), data_dir);

    match action {
        UserAction::Create { username, prefix } => {
            let user = store
                .create_user(&NewUser {
                    username: username.clone(),
                    password_hash: String::new(),
                })
                .await?;

            let token = store.create_api_token(&user.id, Some("default")).await?;

            tracing::info!(
                target: "audit",
                action = "user.create",
                source = "cli",
                client_ip = "local",
                user_id = %user.id,
                username = %user.username,
            );

            tracing::info!(
                target: "audit",
                action = "token.create",
                source = "cli",
                client_ip = "local",
                user_id = %user.id,
                label = "default",
            );

            crate::views::defaults::reconcile_default_views(store.as_ref(), &user.id).await?;

            // Assign task-key prefix. --prefix wins; if absent, derive
            // from username + a snapshot of taken prefixes. The
            // race-defence in `apply_prefix` maps UNIQUE collisions to
            // PREFIX_TAKEN.
            let final_prefix = match prefix {
                Some(supplied) => {
                    let cleaned = supplied.trim();
                    validate_prefix_format(cleaned)
                        .map_err(|msg| anyhow::anyhow!("INVALID_PREFIX: {msg}"))?;
                    apply_prefix(store.as_ref(), &user.id, cleaned, SOURCE_SIGNUP).await?;
                    cleaned.to_string()
                }
                None => {
                    let users = store.list_users().await?;
                    let mut taken: HashSet<String> = HashSet::new();
                    for u in &users {
                        if let Some(p) = store.get_user_prefix(&u.id).await? {
                            taken.insert(p);
                        }
                    }
                    let user_uuid = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
                    let derived = derive_prefix(&user.username, &user_uuid, &taken);
                    apply_prefix(store.as_ref(), &user.id, &derived, SOURCE_SIGNUP).await?;
                    derived
                }
            };

            println!("User created:");
            println!("  ID:       {}", user.id);
            println!("  Username: {}", user.username);
            println!("  Prefix:   {final_prefix}");
            println!("  Created:  {}", user.created_at);
            println!(
                "  Views:    {} defaults seeded",
                crate::views::defaults::default_views().len()
            );
            println!();
            println!("API token (save this — it cannot be retrieved later):");
            println!("  {token}");
        }

        UserAction::SetPrefix { user_id, prefix } => {
            require_user(&store, &user_id).await?;
            let cleaned = prefix.trim();
            validate_prefix_format(cleaned)
                .map_err(|msg| anyhow::anyhow!("INVALID_PREFIX: {msg}"))?;
            apply_prefix(store.as_ref(), &user_id, cleaned, SOURCE_OPERATOR).await?;
            println!("Prefix updated for user {user_id}: {cleaned}");
        }

        UserAction::List => {
            let users = store.list_users().await?;
            if users.is_empty() {
                println!("No users found.");
                return Ok(());
            }
            println!("{:<38} {:<20} CREATED", "ID", "USERNAME");
            println!("{}", "-".repeat(78));
            for user in &users {
                println!("{:<38} {:<20} {}", user.id, user.username, user.created_at);
            }
            println!("\n{} user(s)", users.len());
        }

        UserAction::Delete { user_id, yes } => {
            let user = store
                .get_user_by_id(&user_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("User not found: {user_id}"))?;

            let policy_service = RuntimePolicyService::new(store.clone());
            match policy_service.runtime_delete_for_user(&user.id).await? {
                RuntimeDeleteDecision::Allow => {}
                other => anyhow::bail!(
                    "{}",
                    runtime_delete_message(other).expect("rejected decisions have messages")
                ),
            }

            if !yes
                && !confirm(&format!(
                    "Delete user '{}' ({}) and all associated data? [y/N] ",
                    user.username, user.id
                ))?
            {
                println!("Cancelled.");
                return Ok(());
            }

            store.delete_user(&user.id).await?;

            tracing::info!(
                target: "audit",
                action = "user.delete",
                source = "cli",
                client_ip = "local",
                user_id = %user.id,
                username = %user.username,
            );

            let replica_dir = data_dir.join("users").join(&user.id);
            if replica_dir.exists() {
                std::fs::remove_dir_all(&replica_dir)?;
                println!("Removed replica directory: {}", replica_dir.display());
            }

            println!("Deleted user '{}' ({})", user.username, user.id);
        }

        UserAction::Offline { user_id } => {
            require_user(&store, &user_id).await?;
            recovery.take_user_offline(&user_id, "cli", "local", None);
            println!("User taken offline: {user_id}");
            println!("Running server processes will observe the offline marker shortly and evict cached state.");
        }

        UserAction::Online { user_id } => {
            require_user(&store, &user_id).await?;
            let was_quarantined = recovery.bring_user_online(&user_id, "cli", "local");
            if was_quarantined {
                println!("User brought back online: {user_id}");
            } else {
                println!("User was already online: {user_id}");
            }
        }

        UserAction::Assess { user_id } => {
            require_user(&store, &user_id).await?;
            let assessment = recovery.assess_user_with_source(&user_id, "cli").await?;
            println!("Recovery assessment for user {user_id}:");
            println!("  Status:               {:?}", assessment.status);
            println!("  User dir exists:      {}", assessment.user_dir_exists);
            println!(
                "  Canonical replica:    {}",
                assessment.canonical_replica_exists
            );
            println!(
                "  Sync identity:        {}",
                assessment.sync_identity_exists
            );
            println!(
                "  Merged sync DB:       {}",
                assessment.shared_sync_db_exists
            );
            println!(
                "  Sync schema version:  {} / {}",
                assessment
                    .shared_sync_schema_version
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                assessment.expected_sync_schema_version
            );
            println!("  Devices:              {}", assessment.device_count);
            println!("  Active devices:       {}", assessment.active_device_count);
            if !assessment.missing_device_secrets.is_empty() {
                println!(
                    "  Missing device secrets: {}",
                    assessment.missing_device_secrets.join(", ")
                );
            }
            if assessment.shared_sync_upgrade_needed {
                println!("  Sync uplift needed:   true");
            }
            if let Some(err) = &assessment.shared_sync_db_error {
                println!("  Merged sync DB error: {err}");
            }
            for note in &assessment.notes {
                println!("  Note:                 {note}");
            }
        }
    }
    Ok(())
}
