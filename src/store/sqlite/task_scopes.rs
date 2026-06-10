use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::store::StoreError;

use super::{map_err, store_err_from_anyhow, BoxErr, SqliteConfigStore};
use crate::store::models::{
    PersonalTaskScopeEnsure, TaskScopeKind, TaskScopeRecord, TaskScopeStatus,
    UserMissingPersonalTaskScope,
};

fn task_scope_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskScopeRecord> {
    let kind_raw: String = row.get(1)?;
    let status_raw: String = row.get(5)?;
    let kind = TaskScopeKind::from_db_str(&kind_raw).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(
            1,
            format!("invalid task scope kind {kind_raw:?}"),
            rusqlite::types::Type::Text,
        )
    })?;
    let status = TaskScopeStatus::from_db_str(&status_raw).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(
            5,
            format!("invalid task scope status {status_raw:?}"),
            rusqlite::types::Type::Text,
        )
    })?;
    Ok(TaskScopeRecord {
        id: row.get(0)?,
        kind,
        owner_runtime_user_id: row.get(2)?,
        owner_team_id: row.get(3)?,
        key_prefix: row.get(4)?,
        status,
        storage_path: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

impl SqliteConfigStore {
    pub(super) async fn list_users_pending_personal_task_scope_impl(
        &self,
    ) -> Result<Vec<UserMissingPersonalTaskScope>, StoreError> {
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT users.id, users.username, users.prefix
                     FROM users
                     WHERE users.prefix IS NOT NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM task_scopes
                           WHERE task_scopes.kind = 'personal'
                             AND task_scopes.owner_runtime_user_id = users.id
                             AND task_scopes.status != 'deleted'
                       )
                     ORDER BY users.created_at, users.id",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(UserMissingPersonalTaskScope {
                            id: row.get(0)?,
                            username: row.get(1)?,
                            prefix: row.get(2)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn get_personal_task_scope_for_user_impl(
        &self,
        user_id: &str,
    ) -> Result<Option<TaskScopeRecord>, StoreError> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, kind, owner_runtime_user_id, owner_team_id, key_prefix,
                                status, storage_path, created_at, updated_at
                         FROM task_scopes
                         WHERE kind = 'personal'
                           AND owner_runtime_user_id = ?1
                           AND status != 'deleted'",
                        [&user_id],
                        task_scope_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn lookup_task_scope_by_prefix_for_user_impl(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<Option<TaskScopeRecord>, StoreError> {
        let user_id = user_id.to_string();
        let prefix = prefix.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, kind, owner_runtime_user_id, owner_team_id, key_prefix,
                                status, storage_path, created_at, updated_at
                         FROM task_scopes
                         WHERE kind = 'personal'
                           AND owner_runtime_user_id = ?1
                           AND key_prefix = ?2
                           AND status = 'active'",
                        rusqlite::params![user_id, prefix],
                        task_scope_from_row,
                    )
                    .optional()?;
                Ok::<_, BoxErr>(row)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;
        Ok(result)
    }

    pub(super) async fn ensure_personal_task_scope_for_user_impl(
        &self,
        user_id: &str,
    ) -> Result<PersonalTaskScopeEnsure, StoreError> {
        let user_id_owned = user_id.to_string();
        let user_id = user_id_owned.clone();
        let generated_id = format!("ts_{}", Uuid::new_v4().simple());

        enum EnsureOutcome {
            Scope(PersonalTaskScopeEnsure),
            MissingPrefix,
            PrefixCollision,
        }

        let result = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let prefix: Option<String> = tx
                    .query_row("SELECT prefix FROM users WHERE id = ?1", [&user_id], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .flatten();
                let Some(prefix) = prefix else {
                    tx.commit()?;
                    return Ok::<_, BoxErr>(EnsureOutcome::MissingPrefix);
                };

                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO task_scopes
                        (id, kind, owner_runtime_user_id, key_prefix, status)
                     VALUES (?1, 'personal', ?2, ?3, 'active')",
                    rusqlite::params![generated_id, user_id, prefix],
                )?;

                let row = tx
                    .query_row(
                        "SELECT id, kind, owner_runtime_user_id, owner_team_id, key_prefix,
                                status, storage_path, created_at, updated_at
                         FROM task_scopes
                         WHERE kind = 'personal'
                           AND owner_runtime_user_id = ?1
                           AND status != 'deleted'",
                        [&user_id],
                        task_scope_from_row,
                    )
                    .optional()?;
                let outcome = if let Some(row) = row {
                    EnsureOutcome::Scope(PersonalTaskScopeEnsure {
                        scope: row,
                        created: inserted > 0,
                    })
                } else {
                    EnsureOutcome::PrefixCollision
                };
                tx.commit()?;
                Ok::<_, BoxErr>(outcome)
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)?;

        match result {
            EnsureOutcome::Scope(scope) => Ok(scope),
            EnsureOutcome::MissingPrefix => Err(StoreError::Other(anyhow::anyhow!(
                "user {user_id_owned} has no prefix; cannot ensure Personal Task Scope"
            ))),
            EnsureOutcome::PrefixCollision => Err(StoreError::unique(
                crate::store::error::resources::TASK_SCOPES_KEY_PREFIX,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::store::error::resources;
    use crate::store::models::{NewUser, TaskScopeKind, TaskScopeStatus};
    use crate::store::{ConfigStore, StoreError};

    use super::{map_err, store_err_from_anyhow, BoxErr, SqliteConfigStore};

    struct TestStore {
        store: Arc<SqliteConfigStore>,
        _tmp: tempfile::TempDir,
    }

    async fn migrated_store() -> TestStore {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("config.sqlite");
        let store = Arc::new(SqliteConfigStore::new(&db.to_string_lossy()).await.unwrap());
        store.run_migrations().await.unwrap();
        TestStore { store, _tmp: tmp }
    }

    #[tokio::test]
    async fn ensure_personal_task_scope_is_idempotent() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user = store
            .create_user(&NewUser {
                username: "scope-user".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
        store.set_user_prefix(&user.id, "SCOPE").await.unwrap();

        let first = store
            .ensure_personal_task_scope_for_user(&user.id)
            .await
            .unwrap();
        let second = store
            .ensure_personal_task_scope_for_user(&user.id)
            .await
            .unwrap();

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.scope.id, second.scope.id);
        assert_eq!(
            first.scope.owner_runtime_user_id.as_deref(),
            Some(user.id.as_str())
        );
        assert_eq!(first.scope.key_prefix, "SCOPE");
        assert_eq!(first.scope.kind, TaskScopeKind::Personal);
        assert_eq!(first.scope.status, TaskScopeStatus::Active);
    }

    #[tokio::test]
    async fn ensure_personal_task_scope_is_concurrency_safe() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user = store
            .create_user(&NewUser {
                username: "scope-race".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
        store.set_user_prefix(&user.id, "RACE").await.unwrap();

        let mut joins = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let user_id = user.id.clone();
            joins.push(tokio::spawn(async move {
                store
                    .ensure_personal_task_scope_for_user(&user_id)
                    .await
                    .unwrap()
                    .scope
                    .id
            }));
        }

        let ids = futures::future::try_join_all(joins).await.unwrap();
        assert!(ids.iter().all(|id| id == &ids[0]));
        let scope = store
            .get_personal_task_scope_for_user(&user.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scope.id, ids[0]);
    }

    #[tokio::test]
    async fn ensure_personal_task_scope_requires_prefix() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user = store
            .create_user(&NewUser {
                username: "scope-no-prefix".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();

        let err = store
            .ensure_personal_task_scope_for_user(&user.id)
            .await
            .expect_err("missing prefix must fail");
        assert!(err.to_string().contains("has no prefix"));
    }

    #[tokio::test]
    async fn task_scope_partial_unique_owner_maps_to_resource() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user = store
            .create_user(&NewUser {
                username: "scope-owner-map".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
        store.set_user_prefix(&user.id, "OWNERMAP").await.unwrap();
        let user_id = user.id.clone();
        let err = store
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO task_scopes (id, kind, owner_runtime_user_id, key_prefix, status)
                     VALUES ('ts_owner_a', 'personal', ?1, 'OWNERMAP', 'active')",
                    [&user_id],
                )?;
                conn.execute(
                    "INSERT INTO task_scopes (id, kind, owner_runtime_user_id, key_prefix, status)
                     VALUES ('ts_owner_b', 'personal', ?1, 'OWNERMAP2', 'active')",
                    [&user_id],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)
            .expect_err("duplicate owner must fail");
        assert!(matches!(
            err,
            StoreError::Constraint(crate::store::ConstraintKind::Unique { resource })
                if resource == resources::TASK_SCOPES_PERSONAL_OWNER
        ));
    }

    #[tokio::test]
    async fn task_scope_partial_unique_prefix_maps_to_resource() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user_a = store
            .create_user(&NewUser {
                username: "scope-prefix-a".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
        let user_b = store
            .create_user(&NewUser {
                username: "scope-prefix-b".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
        let user_a_id = user_a.id.clone();
        let user_b_id = user_b.id.clone();
        let err = store
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO task_scopes (id, kind, owner_runtime_user_id, key_prefix, status)
                     VALUES ('ts_prefix_a', 'personal', ?1, 'DUPPFX', 'active')",
                    [&user_a_id],
                )?;
                conn.execute(
                    "INSERT INTO task_scopes (id, kind, owner_runtime_user_id, key_prefix, status)
                     VALUES ('ts_prefix_b', 'personal', ?1, 'DUPPFX', 'active')",
                    [&user_b_id],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)
            .map_err(store_err_from_anyhow)
            .expect_err("duplicate key_prefix must fail");
        assert!(matches!(
            err,
            StoreError::Constraint(crate::store::ConstraintKind::Unique { resource })
                if resource == resources::TASK_SCOPES_KEY_PREFIX
        ));
    }

    #[tokio::test]
    async fn delete_user_removes_personal_task_scope() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user = store
            .create_user(&NewUser {
                username: "scope-delete".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
        store.set_user_prefix(&user.id, "DELETE").await.unwrap();
        store
            .ensure_personal_task_scope_for_user(&user.id)
            .await
            .unwrap();

        assert!(store.delete_user(&user.id).await.unwrap());
        assert!(store
            .get_personal_task_scope_for_user(&user.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn direct_user_delete_cascades_personal_task_scope() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user = store
            .create_user(&NewUser {
                username: "scope-cascade".into(),
                password_hash: "hash".into(),
            })
            .await
            .unwrap();
        store.set_user_prefix(&user.id, "CASCADE").await.unwrap();
        store
            .ensure_personal_task_scope_for_user(&user.id)
            .await
            .unwrap();
        let user_id = user.id.clone();
        store
            .conn
            .call(move |conn| {
                conn.execute("DELETE FROM users WHERE id = ?1", [&user_id])?;
                Ok::<_, BoxErr>(())
            })
            .await
            .unwrap();
        assert!(store
            .get_personal_task_scope_for_user(&user.id)
            .await
            .unwrap()
            .is_none());
    }
}
