use rusqlite::{params, OptionalExtension};

use crate::merged_sync_gateway::journal::{GatewayJournalState, GatewayRecoveryStatus};
use crate::store::models::{
    MergedSyncJournalRecord, MergedSyncJournalStateCount, MergedSyncJournalTransition,
    NewMergedSyncJournalAttempt,
};

use super::{map_err, BoxErr, SqliteConfigStore};

fn journal_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MergedSyncJournalRecord> {
    let state_raw: String = row.get(7)?;
    let recovery_raw: String = row.get(8)?;
    let state = GatewayJournalState::from_db_str(&state_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            format!("unknown merged_sync_journal.state {state_raw}").into(),
        )
    })?;
    let recovery_status = GatewayRecoveryStatus::from_db_str(&recovery_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Text,
            format!("unknown merged_sync_journal.recovery_status {recovery_raw}").into(),
        )
    })?;

    Ok(MergedSyncJournalRecord {
        journal_id: row.get(0)?,
        user_id: row.get(1)?,
        client_id: row.get(2)?,
        attempt_id: row.get(3)?,
        parent_version_id: row.get(4)?,
        inbound_history_segment: row.get(5)?,
        merged_version_id: row.get(6)?,
        state,
        recovery_status,
        diagnostic_code: row.get(9)?,
        diagnostic_message: row.get(10)?,
        state_version: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        finalized_at: row.get(14)?,
    })
}

const SELECT_JOURNAL: &str = "SELECT journal_id, user_id, client_id, attempt_id,
       parent_version_id, inbound_history_segment, merged_version_id, state,
       recovery_status, diagnostic_code, diagnostic_message, state_version,
       created_at, updated_at, finalized_at
 FROM merged_sync_journal";

impl SqliteConfigStore {
    pub(super) async fn create_merged_sync_journal_attempt_impl(
        &self,
        attempt: &NewMergedSyncJournalAttempt,
    ) -> anyhow::Result<MergedSyncJournalRecord> {
        let attempt = attempt.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO merged_sync_journal (
                         journal_id, user_id, client_id, attempt_id, parent_version_id,
                         inbound_history_segment, state, recovery_status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'received', 'recoverable')",
                    params![
                        attempt.journal_id,
                        attempt.user_id,
                        attempt.client_id,
                        attempt.attempt_id,
                        attempt.parent_version_id,
                        attempt.inbound_history_segment,
                    ],
                )?;

                let row = conn.query_row(
                    &format!("{SELECT_JOURNAL} WHERE journal_id = ?1"),
                    params![attempt.journal_id],
                    journal_from_row,
                )?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)
    }

    pub(super) async fn transition_merged_sync_journal_impl(
        &self,
        transition: MergedSyncJournalTransition<'_>,
    ) -> anyhow::Result<Option<MergedSyncJournalRecord>> {
        let MergedSyncJournalTransition {
            journal_id,
            attempt_id,
            from_state,
            to_state,
            merged_version_id,
            recovery_status,
            diagnostic,
        } = transition;

        if !from_state.can_transition_to(to_state) {
            anyhow::bail!(
                "illegal merged sync journal transition {} -> {}",
                from_state.as_str(),
                to_state.as_str()
            );
        }

        let journal_id = journal_id.to_string();
        let attempt_id = attempt_id.to_string();
        let merged_version_id = merged_version_id.map(ToOwned::to_owned);
        let diagnostic_code = diagnostic.and_then(|d| d.code.clone());
        let diagnostic_message = diagnostic.and_then(|d| d.message.clone());
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let changed = tx.execute(
                    "UPDATE merged_sync_journal
                     SET state = ?4,
                         merged_version_id = COALESCE(?5, merged_version_id),
                         recovery_status = ?6,
                         diagnostic_code = ?7,
                         diagnostic_message = ?8,
                         state_version = state_version + 1,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                         finalized_at = CASE
                             WHEN ?4 IN ('finalized', 'failed', 'quarantined')
                             THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                             ELSE finalized_at
                         END
                     WHERE journal_id = ?1
                       AND attempt_id = ?2
                       AND state = ?3",
                    params![
                        journal_id,
                        attempt_id,
                        from_state.as_str(),
                        to_state.as_str(),
                        merged_version_id,
                        recovery_status.as_str(),
                        diagnostic_code,
                        diagnostic_message,
                    ],
                )?;
                if changed == 0 {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(None);
                }

                let row = tx.query_row(
                    &format!("{SELECT_JOURNAL} WHERE journal_id = ?1"),
                    params![journal_id],
                    journal_from_row,
                )?;
                tx.commit()?;
                Ok::<_, BoxErr>(Some(row))
            })
            .await
            .map_err(map_err)
    }

    pub(super) async fn get_merged_sync_journal_impl(
        &self,
        journal_id: &str,
    ) -> anyhow::Result<Option<MergedSyncJournalRecord>> {
        let journal_id = journal_id.to_string();
        self.conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        &format!("{SELECT_JOURNAL} WHERE journal_id = ?1"),
                        params![journal_id],
                        journal_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)
    }

    pub(super) async fn list_merged_sync_journal_for_user_impl(
        &self,
        user_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MergedSyncJournalRecord>> {
        let user_id = user_id.to_string();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX).max(1);
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "{SELECT_JOURNAL} WHERE user_id = ?1 ORDER BY updated_at DESC, journal_id DESC LIMIT ?2"
                ))?;
                let rows = stmt
                    .query_map(params![user_id, limit], journal_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)
    }

    pub(super) async fn list_nonterminal_merged_sync_journal_for_user_impl(
        &self,
        user_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MergedSyncJournalRecord>> {
        let user_id = user_id.to_string();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX).max(1);
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "{SELECT_JOURNAL}
                     WHERE user_id = ?1
                       AND state NOT IN ('finalized', 'failed', 'quarantined')
                     ORDER BY created_at ASC, journal_id ASC
                     LIMIT ?2"
                ))?;
                let rows = stmt
                    .query_map(params![user_id, limit], journal_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)
    }

    pub(super) async fn count_merged_sync_journal_states_for_user_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<MergedSyncJournalStateCount>> {
        let user_id = user_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT state, COUNT(*) FROM merged_sync_journal WHERE user_id = ?1 GROUP BY state",
                )?;
                let rows = stmt
                    .query_map(params![user_id], |row| {
                        let state_raw: String = row.get(0)?;
                        let state = GatewayJournalState::from_db_str(&state_raw).ok_or_else(|| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                format!("unknown merged_sync_journal.state {state_raw}").into(),
                            )
                        })?;
                        Ok(MergedSyncJournalStateCount {
                            state,
                            count: row.get(1)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::models::{MergedSyncJournalDiagnostic, NewUser};
    use crate::store::ConfigStore;

    async fn test_store() -> SqliteConfigStore {
        let store = SqliteConfigStore::new(":memory:").await.unwrap();
        store.run_migrations_inline().await.unwrap();
        store
    }

    async fn create_user(store: &SqliteConfigStore) -> String {
        store
            .create_user(&NewUser {
                username: format!("user-{}", uuid::Uuid::new_v4()),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap()
            .id
    }

    fn new_attempt(user_id: String, attempt_id: &str) -> NewMergedSyncJournalAttempt {
        NewMergedSyncJournalAttempt {
            journal_id: uuid::Uuid::new_v4().to_string(),
            user_id,
            client_id: "client-1".to_string(),
            attempt_id: attempt_id.to_string(),
            parent_version_id: uuid::Uuid::nil().to_string(),
            inbound_history_segment: b"journal-test-segment".to_vec(),
        }
    }

    #[tokio::test]
    async fn journal_transitions_are_compare_and_swaped_by_attempt_and_state() {
        let store = test_store().await;
        let user_id = create_user(&store).await;
        let attempt = new_attempt(user_id, "attempt-a");
        let row = store
            .create_merged_sync_journal_attempt(&attempt)
            .await
            .unwrap();
        assert_eq!(row.state, GatewayJournalState::Received);
        assert_eq!(row.recovery_status, GatewayRecoveryStatus::Recoverable);
        assert_eq!(row.state_version, 0);

        let accepted = store
            .transition_merged_sync_journal(MergedSyncJournalTransition {
                journal_id: &attempt.journal_id,
                attempt_id: "attempt-a",
                from_state: GatewayJournalState::Received,
                to_state: GatewayJournalState::MergedVersionAccepted,
                merged_version_id: Some("11111111-1111-4111-8111-111111111111"),
                recovery_status: GatewayRecoveryStatus::NotRequired,
                diagnostic: None,
            })
            .await
            .unwrap()
            .expect("matching attempt/state transitions");
        assert_eq!(accepted.state, GatewayJournalState::MergedVersionAccepted);
        assert_eq!(accepted.state_version, 1);

        let stale_attempt = store
            .transition_merged_sync_journal(MergedSyncJournalTransition {
                journal_id: &attempt.journal_id,
                attempt_id: "attempt-b",
                from_state: GatewayJournalState::MergedVersionAccepted,
                to_state: GatewayJournalState::SourcePlanApplied,
                merged_version_id: None,
                recovery_status: GatewayRecoveryStatus::NotRequired,
                diagnostic: None,
            })
            .await
            .unwrap();
        assert!(stale_attempt.is_none());

        let stale_state = store
            .transition_merged_sync_journal(MergedSyncJournalTransition {
                journal_id: &attempt.journal_id,
                attempt_id: "attempt-a",
                from_state: GatewayJournalState::Received,
                to_state: GatewayJournalState::MergedVersionAccepted,
                merged_version_id: None,
                recovery_status: GatewayRecoveryStatus::NotRequired,
                diagnostic: None,
            })
            .await
            .unwrap();
        assert!(stale_state.is_none());
    }

    #[tokio::test]
    async fn terminal_diagnostics_are_operator_readable() {
        let store = test_store().await;
        let user_id = create_user(&store).await;
        let attempt = new_attempt(user_id, "attempt-a");
        store
            .create_merged_sync_journal_attempt(&attempt)
            .await
            .unwrap();

        let diagnostic = MergedSyncJournalDiagnostic {
            code: Some("codec_failed".to_string()),
            message: Some("unknown TaskChampion operation variant".to_string()),
        };
        let failed = store
            .transition_merged_sync_journal(MergedSyncJournalTransition {
                journal_id: &attempt.journal_id,
                attempt_id: "attempt-a",
                from_state: GatewayJournalState::Received,
                to_state: GatewayJournalState::Failed,
                merged_version_id: None,
                recovery_status: GatewayRecoveryStatus::Failed,
                diagnostic: Some(&diagnostic),
            })
            .await
            .unwrap()
            .expect("transition to failed");

        assert_eq!(failed.state, GatewayJournalState::Failed);
        assert_eq!(failed.recovery_status, GatewayRecoveryStatus::Failed);
        assert_eq!(failed.diagnostic_code.as_deref(), Some("codec_failed"));
        assert!(failed.finalized_at.is_some());

        let loaded = store
            .get_merged_sync_journal(&attempt.journal_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.diagnostic_message.as_deref(),
            diagnostic.message.as_deref()
        );
    }

    #[tokio::test]
    async fn operator_diagnostics_list_recent_and_count_states() {
        let store = test_store().await;
        let user_id = create_user(&store).await;
        let first = new_attempt(user_id.clone(), "attempt-a");
        let second = new_attempt(user_id.clone(), "attempt-b");
        store
            .create_merged_sync_journal_attempt(&first)
            .await
            .unwrap();
        store
            .create_merged_sync_journal_attempt(&second)
            .await
            .unwrap();
        store
            .transition_merged_sync_journal(MergedSyncJournalTransition {
                journal_id: &first.journal_id,
                attempt_id: "attempt-a",
                from_state: GatewayJournalState::Received,
                to_state: GatewayJournalState::Failed,
                merged_version_id: None,
                recovery_status: GatewayRecoveryStatus::Failed,
                diagnostic: None,
            })
            .await
            .unwrap();

        let counts = store
            .count_merged_sync_journal_states_for_user(&user_id)
            .await
            .unwrap();
        assert!(counts
            .iter()
            .any(|row| row.state == GatewayJournalState::Failed && row.count == 1));
        assert!(counts
            .iter()
            .any(|row| row.state == GatewayJournalState::Received && row.count == 1));

        let recent = store
            .list_merged_sync_journal_for_user(&user_id, 1)
            .await
            .unwrap();
        assert_eq!(recent.len(), 1);
    }

    #[tokio::test]
    async fn illegal_backward_transition_is_rejected_before_sql() {
        let store = test_store().await;
        let user_id = create_user(&store).await;
        let attempt = new_attempt(user_id, "attempt-a");
        store
            .create_merged_sync_journal_attempt(&attempt)
            .await
            .unwrap();

        let err = store
            .transition_merged_sync_journal(MergedSyncJournalTransition {
                journal_id: &attempt.journal_id,
                attempt_id: "attempt-a",
                from_state: GatewayJournalState::SourcePlanApplied,
                to_state: GatewayJournalState::MergedVersionAccepted,
                merged_version_id: None,
                recovery_status: GatewayRecoveryStatus::NotRequired,
                diagnostic: None,
            })
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("illegal merged sync journal transition"));
    }
}
