use base64::Engine;

use crate::app_state::AppState;
use crate::crypto;
use crate::store::models::WebhookDeliveryRecord;

use super::{
    targets::{DeliveryScope, DeliveryTarget},
    DELIVERY_LOG_RETENTION_DAYS, DISABLE_AFTER_FAILURES,
};

pub(super) async fn record_failure(
    state: &AppState,
    webhook: &DeliveryTarget,
    delivery: &WebhookDeliveryRecord,
) -> anyhow::Result<()> {
    let failure_state = match webhook.scope {
        DeliveryScope::User => {
            state
                .store
                .mark_webhook_delivery_failed(&webhook.id, DISABLE_AFTER_FAILURES)
                .await?
        }
        DeliveryScope::Admin => {
            state
                .store
                .mark_admin_webhook_delivery_failed(&webhook.id, DISABLE_AFTER_FAILURES)
                .await?
        }
    };

    tracing::error!(
        target: "boundary",
        event = "webhook.delivery_failed",
        webhook_id = %webhook.id,
        delivery_id = %delivery.delivery_id,
        event_id = %delivery.event_id,
        webhook_event = %delivery.event,
        failure_reason = ?delivery.failure_reason,
    );

    if let Some(state) = failure_state {
        if !state.enabled {
            tracing::error!(
                target: "boundary",
                event = "webhook.disabled",
                webhook_id = %webhook.id,
                consecutive_failures = state.consecutive_failures,
            );
        }
    }

    Ok(())
}

pub(super) fn decrypt_secret(state: &AppState, webhook: &DeliveryTarget) -> anyhow::Result<String> {
    let master_key = state
        .config
        .master_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("master key is required for webhook delivery"))?;
    let secret_enc = base64::engine::general_purpose::STANDARD.decode(&webhook.secret_enc)?;
    let secret = crypto::decrypt_secret(&secret_enc, master_key)?;
    Ok(String::from_utf8(secret)?)
}

pub(super) async fn record_delivery(
    state: &AppState,
    webhook: &DeliveryTarget,
    delivery: &WebhookDeliveryRecord,
) -> anyhow::Result<()> {
    match webhook.scope {
        DeliveryScope::User => state.store.record_webhook_delivery(delivery).await,
        DeliveryScope::Admin => state.store.record_admin_webhook_delivery(delivery).await,
    }
}

pub(super) async fn mark_delivery_succeeded(
    state: &AppState,
    webhook: &DeliveryTarget,
) -> anyhow::Result<()> {
    match webhook.scope {
        DeliveryScope::User => {
            state
                .store
                .mark_webhook_delivery_succeeded(&webhook.id)
                .await
        }
        DeliveryScope::Admin => {
            state
                .store
                .mark_admin_webhook_delivery_succeeded(&webhook.id)
                .await
        }
    }
}

pub(super) async fn purge_old_delivery_logs(state: &AppState) {
    if let Err(err) = state
        .store
        .purge_webhook_deliveries_older_than(DELIVERY_LOG_RETENTION_DAYS)
        .await
    {
        tracing::warn!(error = %err, "Failed to purge old webhook delivery logs");
    }
}
