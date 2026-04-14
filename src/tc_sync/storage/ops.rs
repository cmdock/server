use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

pub(super) fn find_tip_on(conn: &rusqlite::Connection, nil_version_id: Uuid) -> Result<Uuid> {
    let from_meta: Option<Vec<u8>> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'latest_version_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(bytes) = from_meta {
        if bytes.len() == 16 {
            if let Ok(uuid) = Uuid::from_slice(&bytes) {
                if uuid != nil_version_id {
                    return Ok(uuid);
                }
            }
        }
    }

    // Metadata missing — scan versions table for the tip.
    // Only warn if versions exist (empty DB = normal first use, not corruption).
    let has_versions: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM versions)", [], |row| {
            row.get(0)
        })?;
    if has_versions {
        tracing::warn!("Sync metadata missing/corrupt — falling back to version scan");
    }
    let tip: Option<Vec<u8>> = conn
        .query_row(
            "SELECT v.version_id FROM versions v
             LEFT JOIN versions c ON c.parent_version_id = v.version_id
             WHERE c.version_id IS NULL
             ORDER BY v.seq DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;

    match tip {
        Some(ref bytes) if bytes.len() == 16 => {
            let uuid = Uuid::from_slice(bytes).unwrap_or(nil_version_id);
            if uuid != nil_version_id {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO metadata (key, value) VALUES ('latest_version_id', ?1)",
                    params![bytes.as_slice()],
                );
                tracing::info!("Repaired sync metadata: latest_version_id = {uuid}");
            }
            Ok(uuid)
        }
        _ => Ok(nil_version_id),
    }
}
