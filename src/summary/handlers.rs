use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::app_state::AppState;
use crate::auth::AuthUser;
use crate::metrics as m;
use crate::replica;
use crate::tasks::filter;
use crate::user_runtime::{handle_replica_error, open_user_replica};

use super::llm;

#[derive(Deserialize, IntoParams)]
pub struct SummaryQuery {
    /// Summary type: today, overdue, week, morning
    #[serde(rename = "type")]
    pub summary_type: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "type": "today",
    "generated_at": "2026-03-27T08:00:00Z",
    "task_count": 7,
    "summary": "You have 7 tasks due today. 3 are work-related including a high-priority PR review."
}))]
pub struct SummaryResponse {
    /// Summary type: today, overdue, week, morning
    #[serde(rename = "type")]
    pub summary_type: String,
    /// ISO 8601 timestamp of when the summary was generated
    #[schema(format = "date-time")]
    pub generated_at: String,
    pub task_count: usize,
    pub summary: String,
}

/// Filter expression for each summary type.
fn filter_for_summary_type(summary_type: &str) -> &str {
    match summary_type {
        "today" => "status:pending +DUETODAY",
        "overdue" => "status:pending +OVERDUE",
        "week" => "status:pending +WEEK",
        "morning" => "status:pending +DUE",
        _ => "status:pending",
    }
}

/// Get an LLM-generated task summary.
///
/// Fetches tasks matching the summary type, then generates a natural
/// language summary via Claude Haiku. Falls back to a template if the
/// LLM is unavailable or the API key is not configured.
#[utoipa::path(
    get,
    path = "/api/summary",
    operation_id = "getSummary",
    params(SummaryQuery),
    responses(
        (status = 200, description = "Task summary", body = SummaryResponse),
        (status = 401, description = "Unauthorised"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "summary"
)]
#[tracing::instrument(skip_all, fields(user_id = %auth.user_id, summary_type))]
pub async fn get_summary(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(query): Query<SummaryQuery>,
) -> Result<Json<SummaryResponse>, StatusCode> {
    let summary_type = query.summary_type.unwrap_or_else(|| "today".to_string());

    // 1. Fetch tasks matching the summary type
    let rep_arc = open_user_replica(&state, &auth.user_id, "api").await?;

    let filter_str = filter_for_summary_type(&summary_type);
    let mut rep = rep_arc.lock().await;
    let all_tasks = rep
        .all_tasks()
        .await
        .map_err(|e| handle_replica_error(&state, &auth.user_id, &e, "all_tasks", "api"))?;
    drop(rep); // Release lock before LLM call

    let pending_uuids: std::collections::HashSet<uuid::Uuid> = all_tasks
        .values()
        .filter(|t| t.get_status() == taskchampion::Status::Pending)
        .map(|t| t.get_uuid())
        .collect();
    let parsed_filter = filter::parse_filter(filter_str);
    let eval_ctx = filter::EvalCtx::new();
    let matching_tasks: Vec<_> = all_tasks
        .values()
        .filter(|t| filter::matches_with_context(t, &parsed_filter, &eval_ctx))
        .map(|t| replica::task_to_item(t, Some(&pending_uuids)))
        .collect();

    let task_count = matching_tasks.len();

    // 2. Generate summary
    let summary = if task_count == 0 {
        format!("No {summary_type} tasks. You're all clear!")
    } else {
        // Try LLM first, fall back to template
        // Check circuit breaker before calling LLM
        match state.llm_circuit_breaker.check().await {
            Ok(()) => {
                match try_llm_summary(&state, &auth.user_id, &matching_tasks, &summary_type).await {
                    Ok(text) => {
                        state.llm_circuit_breaker.record_success().await;
                        text
                    }
                    Err(e) => {
                        state.llm_circuit_breaker.record_failure().await;
                        tracing::warn!("LLM summary failed, using template: {e}");
                        m::record_llm_fallback();
                        llm::template_summary(task_count, &summary_type)
                    }
                }
            }
            Err(reason) => {
                tracing::debug!("LLM circuit open, using template: {reason}");
                m::record_llm_fallback();
                llm::template_summary(task_count, &summary_type)
            }
        }
    };

    Ok(Json(SummaryResponse {
        summary_type,
        generated_at: chrono::Utc::now().to_rfc3339(),
        task_count,
        summary,
    }))
}

/// Attempt to generate an LLM summary. Returns Err if API key is missing
/// or the API call fails.
async fn try_llm_summary(
    state: &AppState,
    user_id: &str,
    tasks: &[crate::tasks::models::TaskItem],
    summary_type: &str,
) -> anyhow::Result<String> {
    let llm_config = state
        .config
        .llm
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("LLM not configured"))?;

    let api_key = std::env::var(&llm_config.api_key_env)
        .map_err(|_| anyhow::anyhow!("{} env var not set", llm_config.api_key_env))?;

    // Serialise tasks to compact JSON for the prompt
    let tasks_json = serde_json::to_string(tasks)?;

    // Build context hint from user's contexts (if any)
    let context_hint = match state.store.list_contexts(user_id).await {
        Ok(contexts) if !contexts.is_empty() => {
            let ctx_desc: Vec<String> = contexts
                .iter()
                .map(|c| {
                    format!(
                        "{}: projects starting with {:?}",
                        c.label, c.project_prefixes
                    )
                })
                .collect();
            format!(
                "The user has these context groupings: {}. Use them to categorise tasks in your summary.",
                ctx_desc.join("; ")
            )
        }
        _ => String::new(),
    };

    llm::generate_summary(
        &api_key,
        &llm_config.model,
        &tasks_json,
        summary_type,
        &context_hint,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_for_summary_type_today() {
        assert_eq!(filter_for_summary_type("today"), "status:pending +DUETODAY");
    }

    #[test]
    fn test_filter_for_summary_type_overdue() {
        assert_eq!(
            filter_for_summary_type("overdue"),
            "status:pending +OVERDUE"
        );
    }

    #[test]
    fn test_filter_for_summary_type_week() {
        assert_eq!(filter_for_summary_type("week"), "status:pending +WEEK");
    }

    #[test]
    fn test_filter_for_summary_type_morning() {
        assert_eq!(filter_for_summary_type("morning"), "status:pending +DUE");
    }

    #[test]
    fn test_filter_for_summary_type_unknown() {
        assert_eq!(filter_for_summary_type("custom"), "status:pending");
    }
}
