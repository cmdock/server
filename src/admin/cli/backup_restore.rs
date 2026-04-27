use std::collections::HashSet;
use std::path::Path;

use crate::admin::services::recovery::RecoveryCoordinator;
use crate::recovery::{RecoveryStatus, UserRecoveryAssessment};

use super::common::open_store;

pub(super) async fn run_backup(data_dir: &Path, output: &Path) -> anyhow::Result<()> {
    if !data_dir.exists() {
        anyhow::bail!("Data directory not found: {}", data_dir.display());
    }

    eprintln!("Note: For consistent replica backups, stop the server first.");

    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let backup_dir = output.join(format!("backup-{timestamp}"));
    std::fs::create_dir_all(&backup_dir)?;

    let config_db = data_dir.join("config.sqlite");
    if config_db.exists() {
        let db_str = config_db.to_string_lossy().to_string();
        tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&db_str)?;
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok::<_, rusqlite::Error>(())
        })
        .await??;

        std::fs::copy(&config_db, backup_dir.join("config.sqlite"))?;
        println!("Backed up config database");
    }

    for suffix in &["-wal", "-shm"] {
        let wal_path = data_dir.join(format!("config.sqlite{suffix}"));
        if wal_path.exists() {
            std::fs::copy(&wal_path, backup_dir.join(format!("config.sqlite{suffix}")))?;
        }
    }

    let users_dir = data_dir.join("users");
    if users_dir.exists() {
        let backup_users = backup_dir.join("users");
        std::fs::create_dir_all(&backup_users)?;
        let mut user_count = 0u32;
        for entry in std::fs::read_dir(&users_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let dest = backup_users.join(entry.file_name());
                copy_dir_recursive(&entry.path(), &dest)?;
                user_count += 1;
            }
        }
        println!("Backed up {user_count} user replica(s)");
    }

    println!("\nBackup complete: {}", backup_dir.display());
    Ok(())
}

fn sqlite_column_exists(
    conn: &rusqlite::Connection,
    db_name: &str,
    table: &str,
    column: &str,
) -> bool {
    conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {db_name}.pragma_table_info('{table}') WHERE name = ?1)"
        ),
        [column],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn sqlite_table_exists(
    conn: &rusqlite::Connection,
    db_name: &str,
    table: &str,
) -> anyhow::Result<bool> {
    match db_name {
        "main" => Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )?),
        "backup" => Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM backup.sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )?),
        _ => anyhow::bail!("unsupported attached database name: {db_name}"),
    }
}

/// All user-owned tables that single-user restore should clean and repopulate.
/// Order matters: tables with FKs referencing other user tables should appear
/// after the table they reference (delete in forward order, insert in forward order).
const USER_OWNED_TABLES: &[&str] = &[
    "api_tokens",
    "views",
    "contexts",
    "presets",
    "stores",
    "shopping_config",
    "config",
    "replicas",
    "devices",
    "geofences",
    "webhooks",
    "webhook_event_history",
    "user_runtime_policies",
];

fn delete_user_rows_from_table(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    user_id: &str,
) -> anyhow::Result<()> {
    tx.execute(
        &format!("DELETE FROM \"{table}\" WHERE user_id = ?1"),
        [user_id],
    )?;
    Ok(())
}

fn insert_user_rows_from_backup(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    user_id: &str,
) -> anyhow::Result<()> {
    match table {
        "api_tokens" => tx.execute(
            "INSERT INTO api_tokens SELECT * FROM backup.api_tokens WHERE user_id = ?1",
            [user_id],
        )?,
        "views" => {
            // Use explicit columns so pre-022 backups (no context_id) restore cleanly.
            let context_id_expr = if sqlite_column_exists(tx, "backup", "views", "context_id") {
                "context_id"
            } else {
                "NULL"
            };
            tx.execute(
                &format!(
                    "INSERT INTO views (id, user_id, label, icon, filter, group_by,
                                        context_filtered, display_mode, sort_order,
                                        origin, user_modified, hidden, template_version, context_id)
                     SELECT id, user_id, label, icon, filter, group_by,
                            context_filtered, display_mode, sort_order,
                            origin, user_modified, hidden, template_version, {context_id_expr}
                     FROM backup.views WHERE user_id = ?1"
                ),
                [user_id],
            )?
        }
        "contexts" => tx.execute(
            "INSERT INTO contexts SELECT * FROM backup.contexts WHERE user_id = ?1",
            [user_id],
        )?,
        "presets" => tx.execute(
            "INSERT INTO presets SELECT * FROM backup.presets WHERE user_id = ?1",
            [user_id],
        )?,
        "stores" => tx.execute(
            "INSERT INTO stores SELECT * FROM backup.stores WHERE user_id = ?1",
            [user_id],
        )?,
        "shopping_config" => tx.execute(
            "INSERT INTO shopping_config SELECT * FROM backup.shopping_config WHERE user_id = ?1",
            [user_id],
        )?,
        "config" => tx.execute(
            "INSERT INTO config SELECT * FROM backup.config WHERE user_id = ?1",
            [user_id],
        )?,
        "replicas" => tx.execute(
            "INSERT INTO replicas SELECT * FROM backup.replicas WHERE user_id = ?1",
            [user_id],
        )?,
        "devices" => tx.execute(
            "INSERT INTO devices SELECT * FROM backup.devices WHERE user_id = ?1",
            [user_id],
        )?,
        "geofences" => tx.execute(
            "INSERT INTO geofences SELECT * FROM backup.geofences WHERE user_id = ?1",
            [user_id],
        )?,
        // Newer tables — no schema drift concern, safe to use SELECT *.
        "webhooks" | "webhook_event_history" | "user_runtime_policies" => tx.execute(
            &format!("INSERT INTO \"{table}\" SELECT * FROM backup.\"{table}\" WHERE user_id = ?1"),
            [user_id],
        )?,
        _ => anyhow::bail!("unsupported restore table: {table}"),
    };
    Ok(())
}

fn restore_single_user_config(
    target_db: &Path,
    backup_db: &Path,
    user_id: &str,
) -> anyhow::Result<()> {
    let conn = rusqlite::Connection::open(target_db)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    let backup_db = backup_db.to_string_lossy().to_string();
    conn.execute("ATTACH DATABASE ?1 AS backup", [&backup_db])?;

    let backup_has_user: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM backup.users WHERE id = ?1)",
        [user_id],
        |row| row.get(0),
    )?;
    if !backup_has_user {
        anyhow::bail!("Backup does not contain user {user_id}");
    }

    let tx = conn.unchecked_transaction()?;

    // Delete from all user-owned tables that exist in target (clears stale FK rows
    // even when the backup is older and doesn't have those tables).
    for table in USER_OWNED_TABLES {
        if sqlite_table_exists(&tx, "main", table)? {
            delete_user_rows_from_table(&tx, table, user_id)?;
        }
    }
    tx.execute("DELETE FROM users WHERE id = ?1", [user_id])?;
    tx.execute(
        "INSERT INTO users SELECT * FROM backup.users WHERE id = ?1",
        [user_id],
    )?;
    // Insert from all user-owned tables that exist in both target and backup.
    for table in USER_OWNED_TABLES {
        if sqlite_table_exists(&tx, "main", table)? && sqlite_table_exists(&tx, "backup", table)? {
            insert_user_rows_from_backup(&tx, table, user_id)?;
        }
    }

    tx.commit()?;
    conn.execute_batch("DETACH DATABASE backup")?;
    Ok(())
}

fn restore_single_user_files(data_dir: &Path, input: &Path, user_id: &str) -> anyhow::Result<()> {
    let users_dir = data_dir.join("users");
    std::fs::create_dir_all(&users_dir)?;
    let dest = users_dir.join(user_id);
    let marker_was_present = dest.join(".offline").exists();
    let backup_user_dir = input.join("users").join(user_id);

    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    if backup_user_dir.exists() {
        copy_dir_recursive(&backup_user_dir, &dest)?;
    } else {
        std::fs::create_dir_all(&dest)?;
    }
    if marker_was_present {
        std::fs::write(dest.join(".offline"), b"offline\n")?;
    }
    Ok(())
}

fn print_recovery_assessment(assessment: &UserRecoveryAssessment) {
    let status = match assessment.status {
        RecoveryStatus::Healthy => "healthy",
        RecoveryStatus::Rebuildable => "rebuildable",
        RecoveryStatus::NeedsOperatorAttention => "needs_operator_attention",
    };
    println!("Recovery assessment:");
    println!("  Status:               {status}");
    println!(
        "  Canonical replica:    {}",
        assessment.canonical_replica_exists
    );
    println!(
        "  Sync identity:        {}",
        assessment.sync_identity_exists
    );
    println!(
        "  Shared sync DB:       {}",
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
    println!("  Active devices:       {}", assessment.active_device_count);
    if assessment.shared_sync_upgrade_needed {
        println!("  Sync uplift needed:   true");
    }
    if !assessment.missing_device_secrets.is_empty() {
        println!(
            "  Missing device secrets: {}",
            assessment.missing_device_secrets.join(", ")
        );
    }
    if let Some(err) = &assessment.shared_sync_db_error {
        println!("  Shared sync DB error: {err}");
    }
    for note in &assessment.notes {
        println!("  Note:                 {note}");
    }
}

pub(super) async fn run_restore(
    data_dir: &Path,
    input: &Path,
    user_id: Option<&str>,
    yes: bool,
) -> anyhow::Result<()> {
    if !input.exists() {
        anyhow::bail!("Backup directory not found: {}", input.display());
    }

    let config_backup = input.join("config.sqlite");
    if !config_backup.exists() {
        anyhow::bail!(
            "No config.sqlite found in backup directory: {}",
            input.display()
        );
    }

    let config_db = data_dir.join("config.sqlite");
    let users_dir_exists = data_dir.join("users").exists()
        && std::fs::read_dir(data_dir.join("users"))
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
    if !yes && (config_db.exists() || users_dir_exists) {
        super::common::confirm(&format!(
            "WARNING: Existing data in {} will be overwritten{} Continue? [y/N] ",
            data_dir.display(),
            user_id
                .map(|id| format!(" for user {id}."))
                .unwrap_or_else(|| ".".to_string())
        ))?
        .then_some(())
        .ok_or_else(|| anyhow::anyhow!("Cancelled."))?;
    }

    if let Some(user_id) = user_id {
        std::fs::create_dir_all(data_dir)?;
        std::fs::create_dir_all(data_dir.join("users"))?;
        let _ = open_store(data_dir).await?;

        let target_db = config_db.clone();
        let backup_db = config_backup.clone();
        let user_id_owned = user_id.to_string();
        tokio::task::spawn_blocking(move || {
            restore_single_user_config(&target_db, &backup_db, &user_id_owned)
        })
        .await??;
        restore_single_user_files(data_dir, input, user_id)?;
        println!("Restored config and replica files for user {user_id}");

        let store = open_store(data_dir).await?;
        let recovery = RecoveryCoordinator::for_local(store.clone(), data_dir);
        let assessment = recovery.assess_user(user_id).await?;
        print_recovery_assessment(&assessment);
        println!("\nSelective restore complete.");
        return Ok(());
    }

    std::fs::create_dir_all(data_dir)?;
    std::fs::create_dir_all(data_dir.join("users"))?;

    std::fs::copy(&config_backup, &config_db)?;
    println!("Restored config database");

    for suffix in &["-wal", "-shm"] {
        let wal_backup = input.join(format!("config.sqlite{suffix}"));
        if wal_backup.exists() {
            std::fs::copy(&wal_backup, data_dir.join(format!("config.sqlite{suffix}")))?;
        }
    }

    let users_backup = input.join("users");
    let users_dir = data_dir.join("users");
    if users_backup.exists() {
        let backup_ids: HashSet<_> = std::fs::read_dir(&users_backup)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name())
            .collect();

        if users_dir.exists() {
            let mut orphan_count = 0u32;
            for entry in std::fs::read_dir(&users_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() && !backup_ids.contains(&entry.file_name()) {
                    std::fs::remove_dir_all(entry.path())?;
                    orphan_count += 1;
                }
            }
            if orphan_count > 0 {
                println!("Removed {orphan_count} orphan replica(s) not in backup");
            }
        }

        let mut user_count = 0u32;
        for entry in std::fs::read_dir(&users_backup)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let dest = users_dir.join(entry.file_name());
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                copy_dir_recursive(&entry.path(), &dest)?;
                user_count += 1;
            }
        }
        println!("Restored {user_count} user replica(s)");
    }

    println!("\nRestore complete. Start the server to verify.");
    Ok(())
}

pub fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ft.is_symlink() {
            continue;
        } else if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
