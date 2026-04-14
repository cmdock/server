use std::collections::HashSet;
use std::time::Instant;

use axum::http::StatusCode;
use taskchampion::{Operations, Status};
use uuid::Uuid;

use super::models::{AddTaskRequest, ModifyTaskRequest, TaskItem};
use super::mutations::{self, TaskMutationAudit, TaskMutationKind};
use crate::app_state::AppState;
use crate::metrics as m;
use crate::replica;
use crate::tasks::parser;
use crate::user_runtime::{handle_replica_error, open_user_replica};

pub struct TaskMutationSuccess {
    pub kind: TaskMutationKind,
    pub uuid: Uuid,
    pub task_item: TaskItem,
    pub changed_fields: Option<Vec<String>>,
    pub audit: TaskMutationAudit,
}

pub async fn add_task(
    state: &AppState,
    user_id: &str,
    body: &AddTaskRequest,
) -> Result<TaskMutationSuccess, StatusCode> {
    let rep_arc = open_user_replica(state, user_id, "api").await?;
    let parsed = parser::parse_raw(&body.raw);
    let uuid = Uuid::new_v4();

    let op_start = Instant::now();
    let result: Result<TaskItem, StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("create_task", lock_wait_start.elapsed().as_secs_f64());
        let mut ops = Operations::new();

        let step_start = Instant::now();
        let mut task = rep
            .create_task(uuid, &mut ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "create_task", "api"))?;
        m::record_task_mutation_step(
            "create_task",
            "create_task",
            step_start.elapsed().as_secs_f64(),
        );

        let step_start = Instant::now();
        task.set_status(Status::Pending, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "set_status", "api"))?;
        m::record_task_mutation_step(
            "create_task",
            "set_status",
            step_start.elapsed().as_secs_f64(),
        );

        let step_start = Instant::now();
        task.set_entry(Some(chrono::Utc::now()), &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "set_entry", "api"))?;
        m::record_task_mutation_step(
            "create_task",
            "set_entry",
            step_start.elapsed().as_secs_f64(),
        );

        let step_start = Instant::now();
        replica::apply_parsed_fields(&mut task, &parsed, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "apply_fields", "api"))?;
        m::record_task_mutation_step(
            "create_task",
            "apply_fields",
            step_start.elapsed().as_secs_f64(),
        );

        let step_start = Instant::now();
        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;
        m::record_task_mutation_step("create_task", "commit", step_start.elapsed().as_secs_f64());

        Ok(replica::task_to_item(&task))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("create_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("create_task", elapsed, "error"),
    }

    let task_item = result?;
    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Create,
        uuid,
        task_item,
        changed_fields: None,
        audit: TaskMutationAudit::Create {
            project: parsed.project.clone(),
            priority: parsed.priority.clone(),
        },
    })
}

pub async fn complete_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
) -> Result<TaskMutationSuccess, StatusCode> {
    let rep_arc = open_user_replica(state, user_id, "api").await?;

    let op_start = Instant::now();
    let result: Result<TaskItem, StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("complete_task", lock_wait_start.elapsed().as_secs_f64());
        let task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?;

        let mut task = task.ok_or(StatusCode::NOT_FOUND)?;
        if task.get_status() != Status::Pending {
            return Err(StatusCode::CONFLICT);
        }

        let mut ops = Operations::new();
        task.done(&mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "complete_task", "api"))?;

        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;

        Ok(replica::task_to_item(&task))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("complete_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("complete_task", elapsed, "error"),
    }

    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Complete,
        uuid,
        task_item: result?,
        changed_fields: None,
        audit: TaskMutationAudit::None,
    })
}

pub async fn undo_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
) -> Result<TaskMutationSuccess, StatusCode> {
    let rep_arc = open_user_replica(state, user_id, "api").await?;

    let op_start = Instant::now();
    let result: Result<(TaskItem, Vec<String>), StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("undo_task", lock_wait_start.elapsed().as_secs_f64());
        let task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?;

        let mut task = task.ok_or(StatusCode::NOT_FOUND)?;
        let before = replica::task_to_item(&task);

        if task.get_status() != Status::Completed {
            return Err(StatusCode::CONFLICT);
        }

        let mut ops = Operations::new();
        task.set_status(Status::Pending, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "undo_task", "api"))?;

        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;

        let after = replica::task_to_item(&task);
        Ok((after.clone(), mutations::changed_fields(&before, &after)))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("undo_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("undo_task", elapsed, "error"),
    }

    let (task_item, changed_fields) = result?;
    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Undo,
        uuid,
        task_item,
        changed_fields: Some(changed_fields),
        audit: TaskMutationAudit::None,
    })
}

pub async fn delete_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
) -> Result<TaskMutationSuccess, StatusCode> {
    let rep_arc = open_user_replica(state, user_id, "api").await?;

    let op_start = Instant::now();
    let result: Result<TaskItem, StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("delete_task", lock_wait_start.elapsed().as_secs_f64());
        let mut task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?
            .ok_or(StatusCode::NOT_FOUND)?;

        let mut ops = Operations::new();
        task.set_status(Status::Deleted, &mut ops)
            .map_err(|e| handle_replica_error(state, user_id, &e, "delete_task", "api"))?;

        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;

        Ok(replica::task_to_item(&task))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("delete_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("delete_task", elapsed, "error"),
    }

    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Delete,
        uuid,
        task_item: result?,
        changed_fields: None,
        audit: TaskMutationAudit::None,
    })
}

pub fn parse_modify_dependencies(
    uuid: Uuid,
    depends: Option<&Vec<String>>,
) -> Result<Option<Vec<Uuid>>, &'static str> {
    let Some(depends) = depends else {
        return Ok(None);
    };

    let mut unique = Vec::with_capacity(depends.len());
    let mut seen = HashSet::with_capacity(depends.len());
    for dep in depends {
        let dep_uuid = Uuid::parse_str(dep).map_err(|_| "invalid_dependency_uuid")?;
        if dep_uuid == uuid {
            return Err("self_dependency");
        }
        if seen.insert(dep_uuid) {
            unique.push(dep_uuid);
        }
    }

    Ok(Some(unique))
}

pub async fn modify_task(
    state: &AppState,
    user_id: &str,
    uuid: Uuid,
    body: &ModifyTaskRequest,
    parsed_depends: Option<Vec<Uuid>>,
) -> Result<TaskMutationSuccess, StatusCode> {
    let rep_arc = open_user_replica(state, user_id, "api").await?;

    let op_start = Instant::now();
    let result: Result<(TaskItem, Vec<String>), StatusCode> = async {
        let lock_wait_start = Instant::now();
        let mut rep = rep_arc.lock().await;
        m::record_replica_lock_wait("modify_task", lock_wait_start.elapsed().as_secs_f64());
        let mut task = rep
            .get_task(uuid)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?
            .ok_or(StatusCode::NOT_FOUND)?;
        let before = replica::task_to_item(&task);

        if task.get_status() == Status::Deleted {
            return Err(StatusCode::CONFLICT);
        }

        let mut ops = Operations::new();

        if let Some(ref desc) = body.description {
            task.set_description(desc.clone(), &mut ops)
                .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))?;
        }

        if let Some(ref project) = body.project {
            task.set_value("project", Some(project.clone()), &mut ops)
                .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))?;
        }

        if let Some(ref priority) = body.priority {
            task.set_priority(priority.clone(), &mut ops)
                .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))?;
        }

        if let Some(ref due_str) = body.due {
            let dt = crate::tasks::filter::dates::parse_date_value(due_str);
            task.set_due(dt, &mut ops)
                .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))?;
        }

        if let Some(ref new_tags) = body.tags {
            let existing_tags: Vec<_> = task.get_tags().filter(|t| t.is_user()).collect();
            for tag in &existing_tags {
                task.remove_tag(tag, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))?;
            }
            for tag_str in new_tags {
                if let Ok(tag) = taskchampion::Tag::try_from(tag_str.as_str()) {
                    task.add_tag(&tag, &mut ops).map_err(|e| {
                        handle_replica_error(state, user_id, &e, "modify_task", "api")
                    })?;
                }
            }
        }

        if let Some(ref new_depends) = parsed_depends {
            for dep_uuid in new_depends {
                if rep
                    .get_task(*dep_uuid)
                    .await
                    .map_err(|e| handle_replica_error(state, user_id, &e, "get_task", "api"))?
                    .is_none()
                {
                    return Err(StatusCode::BAD_REQUEST);
                }
            }

            let existing_deps: Vec<_> = task.get_dependencies().collect();
            for dep in existing_deps {
                task.remove_dependency(dep, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))?;
            }
            for dep in new_depends {
                task.add_dependency(*dep, &mut ops)
                    .map_err(|e| handle_replica_error(state, user_id, &e, "modify_task", "api"))?;
            }
        }

        rep.commit_operations(ops)
            .await
            .map_err(|e| handle_replica_error(state, user_id, &e, "commit", "api"))?;

        let after = replica::task_to_item(&task);
        Ok((after.clone(), mutations::changed_fields(&before, &after)))
    }
    .await;

    let elapsed = op_start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => m::record_replica_op("modify_task", elapsed, "ok"),
        Err(_) => m::record_replica_op("modify_task", elapsed, "error"),
    }

    let (task_item, changed_fields) = result?;
    Ok(TaskMutationSuccess {
        kind: TaskMutationKind::Modify,
        uuid,
        task_item,
        changed_fields: Some(changed_fields),
        audit: TaskMutationAudit::Modify {
            changed_description: body.description.is_some(),
            changed_project: body.project.is_some(),
            changed_priority: body.priority.is_some(),
            changed_due: body.due.is_some(),
            changed_tags: body.tags.is_some(),
            changed_depends: body.depends.is_some(),
        },
    })
}
