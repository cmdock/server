//! Source-replica validation and apply helpers for inbound merged sync versions.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use taskchampion::{Operation, Operations, Status, Tag};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::merged_sync_gateway::codec::WireOp;
use crate::merged_sync_gateway::planner::{
    SourceApplyPlan, VisibleTaskScope, CMDOCK_ACCOUNT_UDA, CMDOCK_KEY_UDA, CMDOCK_TASK_SCOPE_UDA,
};
use crate::store::models::KeyState;

pub(super) async fn validate_source_plan_for_current_source(
    state: &AppState,
    user_id: &str,
    plan: &SourceApplyPlan,
) -> Result<()> {
    let rep_arc = state.replica_manager.get_replica(user_id).await?;
    let mut rep = rep_arc.lock().await;
    let existing = rep.all_tasks().await?;
    for group in &plan.groups {
        let has_create = group
            .operations
            .iter()
            .any(|indexed| matches!(indexed.op, WireOp::Create { .. }));
        if !has_create && !existing.contains_key(&group.task_uuid) {
            anyhow::bail!(
                "inbound operation references unknown task {}",
                group.task_uuid
            );
        }
        for indexed in &group.operations {
            validate_wire_op(&indexed.op)?;
        }
    }
    Ok(())
}

fn validate_wire_op(op: &WireOp) -> Result<()> {
    if let WireOp::Update {
        property, value, ..
    } = op
    {
        if property == CMDOCK_TASK_SCOPE_UDA
            || property == CMDOCK_ACCOUNT_UDA
            || property == CMDOCK_KEY_UDA
        {
            return Ok(());
        }
        if let Some(tag) = property.strip_prefix("tag_") {
            Tag::try_from(tag)?;
        }
        if let Some(dep) = property.strip_prefix("dep_") {
            Uuid::parse_str(dep)?;
        }
        if property == "status" {
            match value.as_deref() {
                Some("pending" | "completed" | "deleted") | None => {}
                Some(other) => anyhow::bail!("unsupported status value {other}"),
            }
        }
    }
    Ok(())
}

pub(super) async fn apply_source_plan(
    state: &AppState,
    user_id: &str,
    scope: &VisibleTaskScope,
    plan: &SourceApplyPlan,
) -> Result<()> {
    let mutation_lock = state.recovery_runtime.task_mutation_lock(user_id);
    let mutation_guard = mutation_lock.lock().await;
    let rep_arc = state.replica_manager.get_replica(user_id).await?;
    let mut rep = rep_arc.lock().await;
    let existing = rep.all_tasks().await?;
    let existing_uuids = existing.keys().copied().collect::<HashSet<_>>();

    let mut latest_update_timestamps: HashMap<(Uuid, String), DateTime<Utc>> = HashMap::new();
    for group in &plan.groups {
        if existing_uuids.contains(&group.task_uuid) {
            for op in rep.get_task_operations(group.task_uuid).await? {
                if let Operation::Update {
                    uuid,
                    property,
                    timestamp,
                    ..
                } = op
                {
                    latest_update_timestamps
                        .entry((uuid, property))
                        .and_modify(|latest| *latest = (*latest).max(timestamp))
                        .or_insert(timestamp);
                }
            }
        }
    }

    let mut ops = Operations::new();
    let mut allocations: Vec<(String, i64, String)> = Vec::new();
    let mut created_keys: HashMap<Uuid, String> = HashMap::new();
    let mut created_uuids = HashSet::new();

    for group in &plan.groups {
        let mut current_values: HashMap<String, Option<String>> = HashMap::new();
        let has_create = group
            .operations
            .iter()
            .any(|indexed| matches!(indexed.op, WireOp::Create { .. }));
        let mut task = match existing.get(&group.task_uuid) {
            Some(task) => task.clone(),
            None if has_create => {
                let uuid_str = group.task_uuid.to_string();
                let (canonical_key, allocation_to_commit) = match existing_allocation_for_task(
                    state,
                    user_id,
                    &scope.key_prefix,
                    group.task_uuid,
                )
                .await?
                {
                    Some(existing) => existing,
                    None => {
                        let (n, attempt_id) = state
                            .store
                            .reserve_task_key_pending_for_uuid(
                                user_id,
                                &scope.key_prefix,
                                &uuid_str,
                            )
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        (format!("{}-{n}", scope.key_prefix), Some((n, attempt_id)))
                    }
                };
                if let Some((n, attempt_id)) = allocation_to_commit {
                    allocations.push((uuid_str, n, attempt_id));
                }
                created_keys.insert(group.task_uuid, canonical_key);
                created_uuids.insert(group.task_uuid);
                rep.create_task(group.task_uuid, &mut ops).await?
            }
            None => {
                // A pure update/delete for an unknown task cannot be applied
                // to source truth without inventing intent. Keep recovery
                // honest by failing rather than silently dropping it.
                anyhow::bail!(
                    "inbound operation references unknown task {}",
                    group.task_uuid
                );
            }
        };

        for indexed in &group.operations {
            apply_wire_op_to_task(
                &mut task,
                &indexed.op,
                &mut ops,
                &mut latest_update_timestamps,
                &mut current_values,
            )?;
        }

        debug_assert!(
            created_uuids.contains(&group.task_uuid) || existing_uuids.contains(&group.task_uuid)
        );
        let mut stamp_ctx = IdentityStampContext {
            state,
            user_id,
            prefix: &scope.key_prefix,
            allocations_to_commit: &mut allocations,
        };
        stamp_server_owned_identity(
            &mut stamp_ctx,
            group.task_uuid,
            &mut task,
            &mut ops,
            created_keys.get(&group.task_uuid).map(String::as_str),
        )
        .await?;
    }

    rep.commit_operations(ops).await?;

    for (_uuid, n, attempt_id) in allocations {
        state
            .store
            .commit_task_key(user_id, &scope.key_prefix, n, &attempt_id)
            .await
            .map_err(|err| anyhow::anyhow!(err))?;
    }
    drop(rep);
    drop(mutation_guard);

    Ok(())
}

fn apply_wire_op_to_task(
    task: &mut taskchampion::Task,
    op: &WireOp,
    ops: &mut Operations,
    latest_update_timestamps: &mut HashMap<(Uuid, String), DateTime<Utc>>,
    current_values: &mut HashMap<String, Option<String>>,
) -> Result<()> {
    match op {
        WireOp::Create { .. } => {}
        WireOp::Delete { .. } => {
            task.set_status(Status::Deleted, ops)?;
        }
        WireOp::Update {
            uuid,
            property,
            value,
            timestamp,
        } => {
            if property == CMDOCK_TASK_SCOPE_UDA
                || property == CMDOCK_ACCOUNT_UDA
                || property == CMDOCK_KEY_UDA
            {
                // User input for these fields is command/correction input, not
                // source truth. The canonical stamp happens after all ops.
                return Ok(());
            }
            if let Some(tag) = property.strip_prefix("tag_") {
                Tag::try_from(tag)?;
            }
            if let Some(dep) = property.strip_prefix("dep_") {
                Uuid::parse_str(dep)?;
            }
            if property == "status" {
                match value.as_deref() {
                    Some("pending" | "completed" | "deleted") | None => {}
                    Some(other) => anyhow::bail!("unsupported status value {other}"),
                }
            }
            let key = (*uuid, property.clone());
            if latest_update_timestamps
                .get(&key)
                .is_some_and(|latest| timestamp < latest)
            {
                return Ok(());
            }
            let old_value = current_values
                .get(property)
                .cloned()
                .unwrap_or_else(|| task.get_value(property).map(ToOwned::to_owned));
            ops.push(Operation::Update {
                uuid: *uuid,
                property: property.clone(),
                old_value,
                value: value.clone(),
                timestamp: *timestamp,
            });
            current_values.insert(property.clone(), value.clone());
            latest_update_timestamps.insert(key, *timestamp);
        }
    }
    Ok(())
}

struct IdentityStampContext<'a> {
    state: &'a AppState,
    user_id: &'a str,
    prefix: &'a str,
    allocations_to_commit: &'a mut Vec<(String, i64, String)>,
}

async fn stamp_server_owned_identity(
    ctx: &mut IdentityStampContext<'_>,
    uuid: Uuid,
    task: &mut taskchampion::Task,
    ops: &mut Operations,
    created_key: Option<&str>,
) -> Result<()> {
    if task.get_value(CMDOCK_TASK_SCOPE_UDA) != Some(ctx.prefix) {
        task.set_value(CMDOCK_TASK_SCOPE_UDA, Some(ctx.prefix.to_string()), ops)?;
    }
    let key = match created_key {
        Some(key) => Some(key.to_string()),
        None => {
            canonical_key_for_existing_task(
                ctx.state,
                ctx.user_id,
                ctx.prefix,
                uuid,
                ctx.allocations_to_commit,
            )
            .await?
        }
    };
    if let Some(key) = key {
        task.set_value(CMDOCK_KEY_UDA, Some(key), ops)?;
    }
    Ok(())
}

async fn canonical_key_for_existing_task(
    state: &AppState,
    user_id: &str,
    prefix: &str,
    uuid: Uuid,
    allocations_to_commit: &mut Vec<(String, i64, String)>,
) -> Result<Option<String>> {
    let uuid_str = uuid.to_string();
    let Some((key, maybe_pending)) =
        existing_allocation_for_task(state, user_id, prefix, uuid).await?
    else {
        return Ok(None);
    };
    if let Some((n, attempt_id)) = maybe_pending {
        if !allocations_to_commit
            .iter()
            .any(|(_, existing_n, existing_attempt)| {
                *existing_n == n && existing_attempt == &attempt_id
            })
        {
            allocations_to_commit.push((uuid_str, n, attempt_id));
        }
    }
    Ok(Some(key))
}

async fn existing_allocation_for_task(
    state: &AppState,
    user_id: &str,
    prefix: &str,
    uuid: Uuid,
) -> Result<Option<(String, Option<(i64, String)>)>> {
    let uuid_str = uuid.to_string();
    let rows = state
        .store
        .lookup_task_keys_for_drift(user_id, std::slice::from_ref(&uuid_str))
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    let Some(row) = rows.into_iter().find(|row| row.task_uuid == uuid_str) else {
        return Ok(None);
    };
    if row.prefix != prefix {
        anyhow::bail!(
            "task {uuid} has allocation prefix {} outside personal scope {prefix}",
            row.prefix
        );
    }
    let pending = if row.state == KeyState::Pending {
        Some((parse_allocated_n(&row.key, prefix)?, row.attempt_id.clone()))
    } else {
        None
    };
    Ok(Some((row.key, pending)))
}

fn parse_allocated_n(key: &str, prefix: &str) -> Result<i64> {
    let suffix = key
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .with_context(|| format!("allocation key {key} does not match prefix {prefix}"))?;
    suffix
        .parse::<i64>()
        .with_context(|| format!("allocation key {key} has invalid numeric suffix"))
}
