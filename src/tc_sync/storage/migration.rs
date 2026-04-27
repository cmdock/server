use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};

pub(super) fn read_schema_version_on(conn: &rusqlite::Connection) -> Result<Option<i64>> {
    let version = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(version)
}

fn write_schema_version_on(conn: &rusqlite::Connection, version: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?1)",
        params![version.to_string()],
    )?;
    Ok(())
}

pub(super) fn upgrade_to_v1(
    conn: &rusqlite::Connection,
    current_schema_version: i64,
) -> Result<()> {
    ensure_versions_seq_column(conn)?;
    ensure_snapshots_seq_column(conn)?;
    ensure_versions_seq_backfill(conn)?;
    ensure_snapshot_seq_backfill(conn)?;
    ensure_latest_seq_metadata(conn)?;
    write_schema_version_on(conn, current_schema_version)?;
    Ok(())
}

fn ensure_versions_seq_column(conn: &rusqlite::Connection) -> Result<()> {
    let versions_has_seq: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('versions') WHERE name = 'seq')",
        [],
        |row| row.get(0),
    )?;
    if !versions_has_seq {
        conn.execute_batch("ALTER TABLE versions ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;")
            .with_context(|| "Migrating versions table: adding seq column")?;
    }
    Ok(())
}

fn ensure_snapshots_seq_column(conn: &rusqlite::Connection) -> Result<()> {
    let snapshots_has_seq: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('snapshots') WHERE name = 'seq')",
        [],
        |row| row.get(0),
    )?;
    if !snapshots_has_seq {
        conn.execute_batch("ALTER TABLE snapshots ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;")
            .with_context(|| "Migrating snapshots table: adding seq column")?;
    }
    Ok(())
}

fn ensure_versions_seq_backfill(conn: &rusqlite::Connection) -> Result<()> {
    let zero_seq_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM versions WHERE seq = 0", [], |row| {
            row.get(0)
        })?;
    if zero_seq_count > 0 {
        conn.execute_batch(
            "WITH RECURSIVE
            root(vid) AS (
                SELECT v.version_id
                FROM versions v
                LEFT JOIN versions p ON p.version_id = v.parent_version_id
                WHERE v.seq = 0
                  AND (v.parent_version_id = X'00000000000000000000000000000000'
                       OR p.version_id IS NULL)
                ORDER BY (v.parent_version_id = X'00000000000000000000000000000000') DESC
                LIMIT 1
            ),
            chain(vid, rn) AS (
                SELECT vid, 1 FROM root
                UNION ALL
                SELECT v.version_id, c.rn + 1
                FROM versions v
                JOIN chain c ON v.parent_version_id = c.vid
            )
            UPDATE versions SET seq = (
                SELECT rn FROM chain WHERE chain.vid = versions.version_id
            ) WHERE seq = 0 AND version_id IN (SELECT vid FROM chain);",
        )
        .with_context(|| "Backfilling seq values for pre-migration rows")?;

        let remaining: i64 =
            conn.query_row("SELECT COUNT(*) FROM versions WHERE seq = 0", [], |row| {
                row.get(0)
            })?;
        if remaining > 0 {
            tracing::warn!(
                "Backfilled versions seq but {remaining} orphaned rows remain at seq=0 \
                 (unreachable from chain root — possible corruption)"
            );
        } else {
            tracing::info!("Backfilled seq for {zero_seq_count} pre-migration version rows");
        }
    }
    Ok(())
}

fn ensure_snapshot_seq_backfill(conn: &rusqlite::Connection) -> Result<()> {
    let snap_needs_backfill: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM snapshots WHERE id = 1 AND seq = 0)",
        [],
        |row| row.get(0),
    )?;
    if snap_needs_backfill {
        let snap_updated: usize = conn.execute(
            "UPDATE snapshots SET seq = COALESCE(
                (SELECT seq FROM versions WHERE versions.version_id = snapshots.version_id),
                seq
            ) WHERE id = 1 AND seq = 0",
            [],
        )?;
        if snap_updated > 0 {
            let snap_seq: i64 = conn
                .query_row("SELECT seq FROM snapshots WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .optional()?
                .unwrap_or(0);
            if snap_seq == 0 {
                tracing::warn!(
                    "Snapshot row exists with seq=0 but referenced version is missing — \
                     possible corruption. Snapshot urgency may be inaccurate until next snapshot upload."
                );
            } else {
                tracing::info!("Backfilled snapshot seq to {snap_seq}");
            }
        }
    }
    Ok(())
}

fn ensure_latest_seq_metadata(conn: &rusqlite::Connection) -> Result<()> {
    let has_latest_seq: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = 'latest_seq')",
        [],
        |row| row.get(0),
    )?;
    if !has_latest_seq {
        let max_seq: i64 =
            conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM versions", [], |row| {
                row.get(0)
            })?;
        if max_seq > 0 {
            conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('latest_seq', ?1)",
                params![max_seq.to_string()],
            )?;
            tracing::info!("Backfilled latest_seq metadata: {max_seq}");
        }
    }
    Ok(())
}
