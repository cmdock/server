use std::path::Path;

use crate::admin::cli::TokenAction;

use super::common::{confirm, open_store};

pub(super) async fn run(action: TokenAction, data_dir: &Path) -> anyhow::Result<()> {
    let store = open_store(data_dir).await?;

    match action {
        TokenAction::Create { user_id, label } => {
            if store.get_user_by_id(&user_id).await?.is_none() {
                anyhow::bail!("User not found: {user_id}");
            }

            let token = store.create_api_token(&user_id, label.as_deref()).await?;

            tracing::info!(
                target: "audit",
                action = "token.create",
                source = "cli",
                client_ip = "local",
                user_id = %user_id,
            );

            println!("Token created (save this — it cannot be retrieved later):");
            println!("  {token}");
            if let Some(label) = &label {
                println!("  Label: {label}");
            }
        }

        TokenAction::List { user_id } => {
            if store.get_user_by_id(&user_id).await?.is_none() {
                anyhow::bail!("User not found: {user_id}");
            }

            let tokens = store.list_api_tokens(&user_id).await?;
            if tokens.is_empty() {
                println!("No tokens found for user {user_id}.");
                return Ok(());
            }
            println!(
                "{:<20} {:<20} {:<20} {:<20} {:<20} LAST_IP",
                "HASH (prefix)", "LABEL", "EXPIRES", "FIRST_USED", "LAST_USED"
            );
            println!("{}", "-".repeat(128));
            for t in &tokens {
                let hash_prefix = &t.token_hash[..16.min(t.token_hash.len())];
                let label = t.label.as_deref().unwrap_or("-");
                let expires = t.expires_at.as_deref().unwrap_or("never");
                let first_used = t.first_used_at.as_deref().unwrap_or("-");
                let last_used = t.last_used_at.as_deref().unwrap_or("-");
                let last_ip = t.last_used_ip.as_deref().unwrap_or("-");
                println!(
                    "{hash_prefix:<20} {label:<20} {expires:<20} {first_used:<20} {last_used:<20} {last_ip}"
                );
            }
            println!("\n{} token(s)", tokens.len());
        }

        TokenAction::Revoke { token_hash, yes } => {
            if !yes
                && !confirm(&format!(
                    "Revoke token with hash prefix '{token_hash}'? [y/N] "
                ))?
            {
                println!("Cancelled.");
                return Ok(());
            }

            let revoked = if token_hash.len() < 64 {
                let users = store.list_users().await?;
                let mut found_hash = None;
                for user in &users {
                    let tokens = store.list_api_tokens(&user.id).await?;
                    for t in &tokens {
                        if t.token_hash.starts_with(&token_hash) {
                            if found_hash.is_some() {
                                anyhow::bail!(
                                    "Ambiguous prefix '{token_hash}' matches multiple tokens. Use a longer prefix."
                                );
                            }
                            found_hash = Some(t.token_hash.clone());
                        }
                    }
                }
                match found_hash {
                    Some(hash) => store.revoke_api_token(&hash).await?,
                    None => false,
                }
            } else {
                store.revoke_api_token(&token_hash).await?
            };

            if revoked {
                tracing::info!(
                    target: "audit",
                    action = "token.revoke",
                    source = "cli",
                    client_ip = "local",
                    token_hash_prefix = %&token_hash[..16.min(token_hash.len())],
                );
                println!("Token revoked.");
            } else {
                anyhow::bail!("Token not found.");
            }
        }
    }
    Ok(())
}
