use std::collections::HashSet;

use crate::app_state::AppState;
use crate::store::models::{AdminWebhookRecord, WebhookRecord};

#[derive(Debug, Clone, Copy)]
pub(super) enum DeliveryScope {
    User,
    Admin,
}

#[derive(Debug, Clone)]
pub(super) struct DeliveryTarget {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub enabled: bool,
    pub secret_enc: String,
    pub scope: DeliveryScope,
}

impl From<WebhookRecord> for DeliveryTarget {
    fn from(value: WebhookRecord) -> Self {
        Self {
            id: value.id,
            url: value.url,
            events: value.events,
            modified_fields: value.modified_fields,
            enabled: value.enabled,
            secret_enc: value.secret_enc,
            scope: DeliveryScope::User,
        }
    }
}

impl From<AdminWebhookRecord> for DeliveryTarget {
    fn from(value: AdminWebhookRecord) -> Self {
        Self {
            id: value.id,
            url: value.url,
            events: value.events,
            modified_fields: value.modified_fields,
            enabled: value.enabled,
            secret_enc: value.secret_enc,
            scope: DeliveryScope::Admin,
        }
    }
}

pub(super) fn matches_webhook(
    webhook: &DeliveryTarget,
    event: &str,
    changed_fields: Option<&[String]>,
) -> bool {
    let matches_event = webhook
        .events
        .iter()
        .any(|candidate| match candidate.as_str() {
            "*" => true,
            "task.*" => event.starts_with("task."),
            exact => exact == event,
        });

    if !matches_event {
        return false;
    }

    if event != "task.modified" {
        return true;
    }

    let Some(required_fields) = webhook.modified_fields.as_ref() else {
        return true;
    };
    let Some(changed_fields) = changed_fields else {
        return true;
    };

    let changed: HashSet<&str> = changed_fields.iter().map(String::as_str).collect();
    required_fields
        .iter()
        .any(|field| changed.contains(field.as_str()))
}

pub(super) async fn matching_targets(
    state: &AppState,
    user_id: &str,
    event: &str,
    changed_fields: Option<&[String]>,
) -> Vec<DeliveryTarget> {
    let mut targets = Vec::new();

    match state.store.list_webhooks(user_id).await {
        Ok(webhooks) => {
            targets.extend(
                webhooks
                    .into_iter()
                    .map(DeliveryTarget::from)
                    .filter(|webhook| webhook.enabled)
                    .filter(|webhook| matches_webhook(webhook, event, changed_fields)),
            );
        }
        Err(err) => {
            tracing::error!(
                user_id = %user_id,
                event = %event,
                error = %err,
                "Failed to list user webhooks"
            );
        }
    }

    match state.store.list_admin_webhooks().await {
        Ok(webhooks) => {
            targets.extend(
                webhooks
                    .into_iter()
                    .map(DeliveryTarget::from)
                    .filter(|webhook| webhook.enabled)
                    .filter(|webhook| matches_webhook(webhook, event, changed_fields)),
            );
        }
        Err(err) => {
            tracing::error!(event = %event, error = %err, "Failed to list admin webhooks");
        }
    }

    targets
}
