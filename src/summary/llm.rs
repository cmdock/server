//! Anthropic API client for LLM-generated task summaries.
//!
//! Uses Claude Haiku for natural language summaries.
//! Falls back to template-based summaries if API is unavailable.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::metrics as m;

const OUTBOUND_TARGET_ANTHROPIC: &str = "anthropic";

fn classify_http_status(status: reqwest::StatusCode) -> &'static str {
    if status.is_client_error() {
        "http_4xx"
    } else if status.is_server_error() {
        "http_5xx"
    } else {
        "http_other"
    }
}

fn classify_reqwest_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_decode() {
        "decode"
    } else if error.is_body() {
        "body"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_builder() {
        "builder"
    } else if error.is_request() {
        "request"
    } else {
        "transport"
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<Message>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicError {
    error: Option<ErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    message: String,
}

/// Generate a task summary using the Anthropic API (Claude Haiku).
///
/// Returns Ok(summary_text) on success, or Err on API failure.
/// Caller should fall back to template_summary on error.
#[tracing::instrument(skip_all, fields(model, summary_type))]
pub async fn generate_summary(
    api_key: &str,
    model: &str,
    tasks_json: &str,
    summary_type: &str,
    context_hint: &str,
) -> anyhow::Result<String> {
    let start = Instant::now();
    let system = format!(
        "You are a task management assistant embedded in a mobile app. \
         Generate a brief, natural-language summary for the user's {summary_type} view. \
         Be concise — 2-3 sentences maximum. \
         Don't list individual tasks. Focus on the big picture: how many tasks, \
         what areas need attention, and any urgent items. \
         {context_hint}"
    );

    let user_message =
        format!("Here are my {summary_type} tasks as JSON. Summarise them for me:\n\n{tasks_json}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&AnthropicRequest {
            model: model.to_string(),
            max_tokens: 256,
            system,
            messages: vec![Message {
                role: "user".to_string(),
                content: user_message,
            }],
        })
        .send()
        .await
        .inspect_err(|error| {
            let duration = start.elapsed().as_secs_f64();
            m::record_llm_request("error", duration);
            m::record_outbound_http_request(OUTBOUND_TARGET_ANTHROPIC, "transport_error", duration);
            m::record_outbound_http_failure(
                OUTBOUND_TARGET_ANTHROPIC,
                classify_reqwest_error(error),
            );
        })?;

    let status = response.status();
    if !status.is_success() {
        let duration = start.elapsed().as_secs_f64();
        m::record_llm_request("error", duration);
        m::record_outbound_http_request(OUTBOUND_TARGET_ANTHROPIC, "http_error", duration);
        m::record_outbound_http_failure(OUTBOUND_TARGET_ANTHROPIC, classify_http_status(status));
        let body = response.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<AnthropicError>(&body) {
            if let Some(detail) = err.error {
                anyhow::bail!("Anthropic API error ({status}): {}", detail.message);
            }
        }
        anyhow::bail!("Anthropic API error ({status}): {body}");
    }

    let body: AnthropicResponse = response.json().await.inspect_err(|error| {
        let duration = start.elapsed().as_secs_f64();
        m::record_llm_request("error", duration);
        m::record_outbound_http_request(OUTBOUND_TARGET_ANTHROPIC, "decode_error", duration);
        m::record_outbound_http_failure(OUTBOUND_TARGET_ANTHROPIC, classify_reqwest_error(error));
    })?;
    let text = body
        .content
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default();

    if text.is_empty() {
        let duration = start.elapsed().as_secs_f64();
        m::record_llm_request("empty", duration);
        m::record_outbound_http_request(OUTBOUND_TARGET_ANTHROPIC, "empty", duration);
        m::record_outbound_http_failure(OUTBOUND_TARGET_ANTHROPIC, "empty");
        anyhow::bail!("Anthropic API returned empty response");
    }

    let duration = start.elapsed().as_secs_f64();
    m::record_llm_request("success", duration);
    m::record_outbound_http_request(OUTBOUND_TARGET_ANTHROPIC, "success", duration);
    Ok(text)
}

/// Build the Anthropic API request body (exposed for testing).
pub fn build_request_body(
    model: &str,
    tasks_json: &str,
    summary_type: &str,
    context_hint: &str,
) -> serde_json::Value {
    let system = format!(
        "You are a task management assistant embedded in a mobile app. \
         Generate a brief, natural-language summary for the user's {summary_type} view. \
         Be concise — 2-3 sentences maximum. \
         Don't list individual tasks. Focus on the big picture: how many tasks, \
         what areas need attention, and any urgent items. \
         {context_hint}"
    );

    let user_message =
        format!("Here are my {summary_type} tasks as JSON. Summarise them for me:\n\n{tasks_json}");

    serde_json::json!({
        "model": model,
        "max_tokens": 256,
        "system": system,
        "messages": [{"role": "user", "content": user_message}]
    })
}

/// Generate a template-based summary (fallback when LLM unavailable).
pub fn template_summary(task_count: usize, summary_type: &str) -> String {
    match summary_type {
        "today" => format!("You have {task_count} tasks due today."),
        "overdue" => format!("You have {task_count} overdue tasks."),
        "week" => format!("You have {task_count} tasks this week."),
        "morning" => format!("Good morning! You have {task_count} tasks to review."),
        _ => format!("You have {task_count} tasks."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_today() {
        let result = template_summary(5, "today");
        assert_eq!(result, "You have 5 tasks due today.");
    }

    #[test]
    fn test_template_overdue() {
        let result = template_summary(3, "overdue");
        assert_eq!(result, "You have 3 overdue tasks.");
    }

    #[test]
    fn test_template_week() {
        let result = template_summary(12, "week");
        assert_eq!(result, "You have 12 tasks this week.");
    }

    #[test]
    fn test_template_morning() {
        let result = template_summary(7, "morning");
        assert_eq!(result, "Good morning! You have 7 tasks to review.");
    }

    #[test]
    fn test_template_unknown_type() {
        let result = template_summary(2, "custom");
        assert_eq!(result, "You have 2 tasks.");
    }

    #[test]
    fn test_template_zero_tasks() {
        let result = template_summary(0, "today");
        assert_eq!(result, "You have 0 tasks due today.");
    }

    #[test]
    fn test_build_request_body_structure() {
        let body = build_request_body(
            "claude-haiku-4-5-20251001",
            r#"[{"uuid":"abc","description":"Test"}]"#,
            "today",
            "",
        );

        assert_eq!(body["model"], "claude-haiku-4-5-20251001");
        assert_eq!(body["max_tokens"], 256);
        assert!(body["system"].as_str().unwrap().contains("today"));
        assert!(body["messages"][0]["role"].as_str().unwrap() == "user");
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Test"));
    }

    #[test]
    fn test_build_request_body_with_context_hint() {
        let body = build_request_body(
            "claude-haiku-4-5-20251001",
            "[]",
            "morning",
            "User has Work and Personal contexts.",
        );

        let system = body["system"].as_str().unwrap();
        assert!(system.contains("morning"));
        assert!(system.contains("Work and Personal"));
    }

    #[test]
    fn test_build_request_body_includes_tasks_in_user_message() {
        let tasks_json = r#"[{"uuid":"x","description":"Buy milk"}]"#;
        let body = build_request_body("claude-haiku-4-5-20251001", tasks_json, "today", "");

        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("Buy milk"));
        assert!(content.contains("today"));
    }

    #[tokio::test]
    async fn test_generate_summary_bad_api_key() {
        // Calling with a fake key should return an error, not panic
        let result = generate_summary(
            "sk-ant-fake-key",
            "claude-haiku-4-5-20251001",
            r#"[{"description":"Test task"}]"#,
            "today",
            "",
        )
        .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Anthropic API error") || err_msg.contains("error"),
            "Expected API error, got: {err_msg}"
        );
    }
}
