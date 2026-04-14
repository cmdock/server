//! Webhook delivery runtime.
//!
//! This module is an intentional webhook orchestrator: it owns target discovery,
//! dispatch preparation, retries, delivery logging, failure accounting, and
//! retention cleanup so those concerns stay out of task, sync, and handler code.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use ring::hmac;
use serde::Serialize;
use uuid::Uuid;

use self::state::{
    decrypt_secret, mark_delivery_succeeded, purge_old_delivery_logs, record_delivery,
    record_failure,
};
use self::targets::{matching_targets, DeliveryTarget};
use crate::app_state::AppState;
use crate::metrics;
use crate::store::models::{
    AdminWebhookRecord, WebhookDeliveryRecord, WebhookRecord, WebhookSyncSummary,
};
use crate::tasks::models::TaskItem;
use crate::webhooks::security;

mod state;
mod targets;

const DISABLE_AFTER_FAILURES: u32 = 10;
const DELIVERY_LOG_RETENTION_DAYS: u32 = 7;

#[derive(Debug, Clone)]
pub struct WebhookDispatchRequest {
    pub url: String,
    pub host: String,
    pub resolved_addrs: Vec<std::net::SocketAddr>,
    pub content_type: String,
    pub signature: String,
    pub request_id: String,
    pub user_agent: String,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct WebhookDispatchResult {
    pub status: u16,
}

#[async_trait]
pub trait WebhookTransport: Send + Sync {
    async fn dispatch(
        &self,
        request: WebhookDispatchRequest,
    ) -> anyhow::Result<WebhookDispatchResult>;
}

#[derive(Debug, Default)]
pub struct ReqwestWebhookTransport;

#[async_trait]
impl WebhookTransport for ReqwestWebhookTransport {
    async fn dispatch(
        &self,
        request: WebhookDispatchRequest,
    ) -> anyhow::Result<WebhookDispatchResult> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(Policy::none())
            .user_agent(request.user_agent)
            .resolve_to_addrs(&request.host, &request.resolved_addrs)
            .build()?;
        let response = client
            .post(request.url)
            .header("content-type", request.content_type)
            .header("x-webhook-signature-256", request.signature)
            .header("x-request-id", request.request_id)
            .body(request.body)
            .send()
            .await?;
        Ok(WebhookDispatchResult {
            status: response.status().as_u16(),
        })
    }
}

#[derive(Debug, Serialize)]
struct DeliveryPayload {
    v: u32,
    event: String,
    event_id: String,
    timestamp: String,
    webhook_id: String,
    delivery_id: String,
    attempt: u32,
    user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<TaskItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_fields: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync: Option<WebhookSyncSummary>,
}

pub async fn emit_task_event(
    state: &AppState,
    user_id: &str,
    event: &str,
    task: TaskItem,
    changed_fields: Option<Vec<String>>,
    request_id: Option<String>,
) {
    deliver_event(
        state,
        user_id,
        event,
        Some(task),
        changed_fields,
        None,
        request_id,
    )
    .await;
}

pub async fn send_test_delivery(
    state: &AppState,
    webhook: &WebhookRecord,
    request_id: Option<String>,
) -> anyhow::Result<WebhookDeliveryRecord> {
    deliver(
        state,
        &DeliveryTarget::from(webhook.clone()),
        &webhook.user_id,
        "webhook.test",
        format!("evt_{}", Uuid::new_v4().simple()),
        None,
        None,
        None,
        request_id,
    )
    .await
}

pub async fn send_admin_test_delivery(
    state: &AppState,
    webhook: &AdminWebhookRecord,
    request_id: Option<String>,
) -> anyhow::Result<WebhookDeliveryRecord> {
    deliver(
        state,
        &DeliveryTarget::from(webhook.clone()),
        "admin",
        "webhook.test",
        format!("evt_{}", Uuid::new_v4().simple()),
        None,
        None,
        None,
        request_id,
    )
    .await
}

pub async fn emit_sync_event(
    state: &AppState,
    user_id: &str,
    summary: WebhookSyncSummary,
    request_id: Option<String>,
) {
    if summary.tasks_changed == 0 {
        return;
    }

    deliver_event(
        state,
        user_id,
        "sync.completed",
        None,
        None,
        Some(summary),
        request_id,
    )
    .await;
}

async fn deliver_event(
    state: &AppState,
    user_id: &str,
    event: &str,
    task: Option<TaskItem>,
    changed_fields: Option<Vec<String>>,
    sync: Option<WebhookSyncSummary>,
    request_id: Option<String>,
) {
    let targets = matching_targets(state, user_id, event, changed_fields.as_deref()).await;
    if targets.is_empty() {
        return;
    }

    let event_id = format!("evt_{}", Uuid::new_v4().simple());
    for target in targets {
        let _ = deliver(
            state,
            &target,
            user_id,
            event,
            event_id.clone(),
            task.clone(),
            changed_fields.clone(),
            sync.clone(),
            request_id.clone(),
        )
        .await;
    }
}

// Delivery context (state, webhook, user_id) and payload (event, task, sync, ...)
// could be split into two structs; tracked as a future refactor.
#[allow(clippy::too_many_arguments)]
async fn deliver(
    state: &AppState,
    webhook: &DeliveryTarget,
    user_id: &str,
    event: &str,
    event_id: String,
    task: Option<TaskItem>,
    changed_fields: Option<Vec<String>>,
    sync: Option<WebhookSyncSummary>,
    request_id: Option<String>,
) -> anyhow::Result<WebhookDeliveryRecord> {
    let target = match security::prepare_target(state, &webhook.url).await {
        Ok(target) => target,
        Err(err) => {
            metrics::record_webhook_delivery(event, "ssrf_blocked", 0.0);
            let delivery = WebhookDeliveryRecord {
                delivery_id: format!("del_{}", Uuid::new_v4().simple()),
                webhook_id: webhook.id.clone(),
                event_id,
                event: event.to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                status: "failed".to_string(),
                response_status: None,
                attempt: 1,
                failure_reason: Some(err.to_string()),
            };
            record_delivery(state, webhook, &delivery).await?;
            purge_old_delivery_logs(state).await;
            record_failure(state, webhook, &delivery).await?;
            return Ok(delivery);
        }
    };
    let secret = decrypt_secret(state, webhook)?;
    let max_attempts = state.webhook_retry_delays.len() + 1;

    for attempt in 1..=max_attempts {
        if attempt > 1 {
            tokio::time::sleep(state.webhook_retry_delays[attempt - 2]).await;
        }

        let delivery_id = format!("del_{}", Uuid::new_v4().simple());
        let delivery_request_id = format!("req_{}", Uuid::new_v4().simple());
        let timestamp = chrono::Utc::now().to_rfc3339();
        let payload = DeliveryPayload {
            v: 1,
            event: event.to_string(),
            event_id: event_id.clone(),
            timestamp: timestamp.clone(),
            webhook_id: webhook.id.clone(),
            delivery_id: delivery_id.clone(),
            attempt: attempt as u32,
            user_id: user_id.to_string(),
            request_id: request_id.clone(),
            task: task.clone(),
            changed_fields: changed_fields.clone(),
            sync: sync.clone(),
        };
        let body = serde_json::to_vec(&payload)?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        let signature = hmac::sign(&key, &body);
        let signature = format!("sha256={}", hex::encode(signature.as_ref()));

        let dispatch_request = WebhookDispatchRequest {
            url: target.url.clone(),
            host: target.host.clone(),
            resolved_addrs: target.resolved_addrs.clone(),
            content_type: "application/json".to_string(),
            signature,
            request_id: delivery_request_id,
            user_agent: format!("cmdock-server/{}", env!("CARGO_PKG_VERSION")),
            body,
        };

        tracing::info!(
            target: "boundary",
            event = "webhook.delivery_attempted",
            webhook_id = %webhook.id,
            delivery_id = %delivery_id,
            event_id = %event_id,
            webhook_event = %event,
            attempt = attempt,
        );

        let started = std::time::Instant::now();
        let outcome = state.webhook_transport.dispatch(dispatch_request).await;
        let mut delivery = WebhookDeliveryRecord {
            delivery_id,
            webhook_id: webhook.id.clone(),
            event_id: event_id.clone(),
            event: event.to_string(),
            timestamp,
            status: "failed".to_string(),
            response_status: None,
            attempt: attempt as u32,
            failure_reason: None,
        };

        match outcome {
            Ok(response) if (200..300).contains(&response.status) => {
                metrics::record_webhook_delivery(
                    event,
                    "delivered",
                    started.elapsed().as_secs_f64(),
                );
                delivery.status = "delivered".to_string();
                delivery.response_status = Some(response.status);
                record_delivery(state, webhook, &delivery).await?;
                purge_old_delivery_logs(state).await;
                mark_delivery_succeeded(state, webhook).await?;
                tracing::info!(
                    target: "boundary",
                    event = "webhook.delivery_succeeded",
                    webhook_id = %webhook.id,
                    delivery_id = %delivery.delivery_id,
                    event_id = %delivery.event_id,
                    webhook_event = %event,
                    attempt = attempt,
                );
                return Ok(delivery);
            }
            Ok(response) => {
                metrics::record_webhook_delivery(
                    event,
                    "http_error",
                    started.elapsed().as_secs_f64(),
                );
                delivery.response_status = Some(response.status);
                delivery.failure_reason = Some(format!("non-success status {}", response.status));
            }
            Err(err) => {
                metrics::record_webhook_delivery(
                    event,
                    "transport_error",
                    started.elapsed().as_secs_f64(),
                );
                delivery.failure_reason = Some(err.to_string());
            }
        }

        record_delivery(state, webhook, &delivery).await?;
        purge_old_delivery_logs(state).await;
        if attempt == max_attempts {
            record_failure(state, webhook, &delivery).await?;
            return Ok(delivery);
        }
    }

    unreachable!("webhook delivery loop must return on success or final failure")
}
