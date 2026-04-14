use std::path::{Path, PathBuf};

use super::{map_err, BoxErr, SqliteConfigStore};

impl SqliteConfigStore {
    fn find_migrations_dir() -> anyhow::Result<PathBuf> {
        let candidates = [
            PathBuf::from("migrations"),
            PathBuf::from("/app/migrations"),
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("migrations")))
                .unwrap_or_default(),
        ];
        candidates
            .iter()
            .find(|p| p.is_dir())
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Migrations directory not found. Searched: {}",
                    candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    pub(super) async fn checkpoint_database_impl(&self) -> anyhow::Result<()> {
        self.conn
            .call(|conn| {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn backup_to_path_impl(&self, dst: &Path) -> anyhow::Result<()> {
        let dst = dst.to_path_buf();
        self.conn
            .call(move |conn| {
                conn.backup(rusqlite::MAIN_DB, &dst, None::<fn(_)>)?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn restore_from_path_impl(&self, src: &Path) -> anyhow::Result<()> {
        let src = src.to_path_buf();
        self.conn
            .call(move |conn| {
                conn.restore(rusqlite::MAIN_DB, &src, None::<fn(_)>)?;
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn run_migrations_impl(&self) -> anyhow::Result<()> {
        let migrations_dir = Self::find_migrations_dir()?;
        let mut entries: Vec<_> = std::fs::read_dir(migrations_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "sql"))
            .collect();
        entries.sort_by_key(|e| e.file_name());

        let mut sqls = Vec::new();
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let sql = std::fs::read_to_string(entry.path())?;
            sqls.push((name, sql));
        }

        self.conn
            .call(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS _migrations (
                        name TEXT PRIMARY KEY,
                        applied_at TEXT NOT NULL DEFAULT (datetime('now'))
                    );",
                )?;

                for (name, sql) in sqls {
                    let already: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM _migrations WHERE name = ?1)",
                        [&name],
                        |r| r.get(0),
                    )?;
                    if already {
                        continue;
                    }

                    let tx = conn.transaction()?;
                    tx.execute_batch(&sql)?;
                    tx.execute("INSERT INTO _migrations (name) VALUES (?1)", [&name])?;
                    tx.commit()?;
                }

                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }
}
