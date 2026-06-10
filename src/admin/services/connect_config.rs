//! Connect-config consolidation service (#123).
//!
//! Single owner of connect-config issuance, TTL handling, audit emission,
//! and first/repeat-use telemetry classification. HTTP and CLI handlers
//! are thin adapters that build a request, call the service, and render
//! the response.
//!
//! The review note (`adr-0002-review-2026-05-04.md` §P3) also scoped
//! "bootstrap recovery" into the consolidation. On closer inspection the
//! bootstrap-recovery branches inside `bootstrap_user_device` are keyed
//! on `bootstrap_request_id` against the `devices` table — they share
//! zero code path with this service's `api_tokens` / `mark_token_used`
//! flow. Folding them in would couple two unrelated abstractions, which
//! is the exact accidental complecting ADR-0002 §HC-2 prevents. They
//! belong with the rest of the `bootstrap_user_device` orchestrator
//! cleanup (#122) and are tracked separately.

use std::sync::Arc;

use chrono::{Duration as ChronoDuration, Utc};

use crate::audit;
use crate::connect_config::{
    build_connect_url_with_scheme, normalize_connect_server_url, DEFAULT_CONNECT_TOKEN_BYTES,
    DEFAULT_CONNECT_TOKEN_TTL_MINUTES,
};
use crate::metrics as m;
use crate::store::models::LabeledTokenCorrelation;
use crate::store::ConfigStore;

/// Canonical label for connect-config tokens in the `api_tokens` table.
/// Single source of truth — the storage layer is generic and just stores
/// whatever label the service supplies. If you need this string anywhere
/// else, route through `ConnectConfigService` instead of duplicating it.
pub const CONNECT_CONFIG_LABEL: &str = "connect-config";

/// Token-id prefix used by the service when minting connect-config rows.
/// Operators see `cc_<hex>` in audit logs and can immediately tell the
/// row was a connect-config issuance without looking up the label column.
pub const CONNECT_CONFIG_TOKEN_ID_PREFIX: &str = "cc_";

/// Generate a fresh connect-config token id (`cc_<16-hex>`). Service-side
/// concern — the storage layer just persists whatever string it's given.
fn new_connect_config_token_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    format!("{CONNECT_CONFIG_TOKEN_ID_PREFIX}{}", hex::encode(bytes))
}

/// Where a connect-config issuance is being requested from. Drives audit
/// field shape (HTTP carries `request_id` + `name`; CLI carries
/// `url_length` and hardcodes `client_ip = "local"`).
#[derive(Debug, Clone)]
pub enum IssueSource {
    /// HTTP admin endpoint (`POST /admin/user/{id}/connect-config`).
    Http {
        client_ip: String,
        request_id: Option<String>,
    },
    /// Standalone admin CLI (`cmdock-server admin connect-config create ...`).
    /// `scheme` controls the connect URL scheme (`cmdock://` or
    /// `cmdock-staging://`).
    Cli { scheme: String },
}

/// Inputs to `ConnectConfigService::issue`.
#[derive(Debug, Clone)]
pub struct IssueRequest {
    pub user_id: String,
    pub display_name: Option<String>,
    pub server_url: String,
    /// Per-call TTL override; `None` falls back to
    /// [`DEFAULT_CONNECT_TOKEN_TTL_MINUTES`].
    pub ttl_minutes: Option<u32>,
    pub source: IssueSource,
}

/// Result of a successful issuance.
#[derive(Debug, Clone)]
pub struct IssueOutcome {
    pub credential: String,
    pub token_id: String,
    pub credential_hash_prefix: String,
    pub server_url: String,
    /// `"%Y-%m-%d %H:%M:%S"` UTC, matching the audit/store format.
    pub expires_at: String,
    /// CLI flows surface a fully-built `cmdock://connect?payload=…` URL
    /// (used for stdout + QR render). HTTP flows return `None` — the
    /// caller embeds the credential in the JSON response and clients
    /// build their own URL.
    pub connect_url: Option<String>,
}

/// Per-request context required to emit the `connect_config.consume`
/// audit event with the same field shape as the pre-refactor middleware
/// path. Service callers (today: auth middleware) build this from the
/// incoming request before calling `record_use`.
#[derive(Debug, Clone)]
pub struct UseContext {
    pub client_ip: String,
    pub request_id: Option<String>,
    pub request_path: String,
}

/// Identity returned with `FirstUse` / `RepeatUse` so the caller can
/// emit boundary events (`connect_config.token_redeemed`,
/// `connection.established`) without re-querying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseIdentity {
    pub user_id: String,
    pub token_id: String,
    pub credential_hash_prefix: String,
    pub expires_at: Option<String>,
}

/// Outcome of `ConnectConfigService::record_use`. Classified by the
/// service based on the row's `label` (canonical
/// [`CONNECT_CONFIG_LABEL`] match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseOutcome {
    FirstUse(UseIdentity),
    RepeatUse(UseIdentity),
    NotConnectConfig,
}

#[derive(Clone)]
pub struct ConnectConfigService {
    store: Arc<dyn ConfigStore>,
}

impl ConnectConfigService {
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self { store }
    }

    /// Issue a connect-config credential.
    ///
    /// Owns the full issuance pipeline:
    ///   1. Normalise `server_url` (https-origin guard).
    ///   2. Compute `expires_at` from `ttl_minutes` (default
    ///      [`DEFAULT_CONNECT_TOKEN_TTL_MINUTES`]).
    ///   3. Insert the row in `api_tokens` via the store.
    ///   4. For CLI, build the connect URL.
    ///   5. Emit the boundary `connect_config.token_issued` event and
    ///      the audit `connect_config.generate` event with the per-source
    ///      field shape.
    pub async fn issue(&self, mut req: IssueRequest) -> anyhow::Result<IssueOutcome> {
        // Normalize display name once at the boundary so audit events,
        // build_connect_url, and any storage-side caller observe the same
        // shape (whitespace-only / empty → None). Pre-#123 the HTTP path
        // ran the same normalization inline; this keeps parity (#123 codex
        // iter1).
        req.display_name =
            crate::connect_config::normalize_optional_display_name(req.display_name.as_deref());

        let server_url = normalize_connect_server_url(&req.server_url)?;
        let ttl_minutes = req
            .ttl_minutes
            .map(i64::from)
            .unwrap_or(DEFAULT_CONNECT_TOKEN_TTL_MINUTES);
        let expires_at = (Utc::now() + ChronoDuration::minutes(ttl_minutes))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        let token_id = new_connect_config_token_id();
        let issued = self
            .store
            .create_labeled_api_token(
                &req.user_id,
                CONNECT_CONFIG_LABEL,
                &token_id,
                &expires_at,
                DEFAULT_CONNECT_TOKEN_BYTES,
            )
            .await?;

        // Fetch the user's task-key prefix (server#130). Best-effort —
        // a missing prefix at issue time means the user existed before
        // the backfill ran (or the operator will set one shortly via
        // `admin user set-prefix`); the field is optional in the
        // payload, so older clients ignore it and newer clients that
        // need it can fall back to the REST API.
        let user_prefix = self
            .store
            .get_user_prefix(&req.user_id)
            .await
            .unwrap_or(None);

        let connect_url = match &req.source {
            IssueSource::Cli { scheme } => Some(build_connect_url_with_scheme(
                &server_url,
                Some(&issued.token_id),
                req.display_name.as_deref(),
                &issued.token,
                user_prefix.as_deref(),
                scheme,
            )?),
            IssueSource::Http { .. } => None,
        };

        emit_token_issued(
            &req,
            &issued.token_id,
            &issued.credential_hash_prefix,
            &issued.expires_at,
        );
        emit_generate_audit(
            &req,
            &issued.token_id,
            &issued.credential_hash_prefix,
            &issued.expires_at,
            connect_url.as_deref(),
        );

        Ok(IssueOutcome {
            credential: issued.token,
            token_id: issued.token_id,
            credential_hash_prefix: issued.credential_hash_prefix,
            server_url,
            expires_at: issued.expires_at,
            connect_url,
        })
    }

    /// Resolve a bearer token against the connect-config label, returning
    /// the row's correlation identity (token_id, credential hash prefix,
    /// expiry, expired-flag). Service-owned wrapper around the generic
    /// [`ConfigStore::lookup_token_correlation`] — auth middleware calls
    /// this to attach connect-config telemetry to incoming requests
    /// without baking the label string into the storage layer.
    ///
    /// Returns `None` for any token whose row is missing or whose label
    /// is not `connect-config`.
    pub async fn lookup_correlation(
        &self,
        bearer_token: &str,
    ) -> anyhow::Result<Option<LabeledTokenCorrelation>> {
        self.store
            .lookup_token_correlation(bearer_token, CONNECT_CONFIG_LABEL)
            .await
    }

    /// Record a connect-config token use.
    ///
    /// Calls into [`ConfigStore::mark_token_used`] (which stamps
    /// `first_used_at` / `last_used_*` and returns the row identity),
    /// then classifies the outcome:
    ///   - row missing → [`UseOutcome::NotConnectConfig`]
    ///   - label != [`CONNECT_CONFIG_LABEL`] → [`UseOutcome::NotConnectConfig`]
    ///   - `was_first_use=true` → emit `connect_config.consume` audit
    ///     event + `first_use` metric, return [`UseOutcome::FirstUse`]
    ///   - otherwise → emit `repeat_use` metric, return
    ///     [`UseOutcome::RepeatUse`]
    ///
    /// Boundary events (`connect_config.token_redeemed`,
    /// `connection.established`) remain the caller's responsibility —
    /// they fire on auth-flow rejection paths the service never sees.
    pub async fn record_use(
        &self,
        bearer_token: &str,
        ctx: &UseContext,
    ) -> anyhow::Result<UseOutcome> {
        // The store filters on `expected_label` and returns `None`
        // both for missing rows and for label mismatches — preserving
        // the pre-refactor behaviour where regular API tokens were
        // never written to on the auth hot path. Either case → the
        // service classifies the call as `NotConnectConfig`.
        let row = self
            .store
            .mark_token_used(bearer_token, &ctx.client_ip, CONNECT_CONFIG_LABEL)
            .await?;
        let Some(row) = row else {
            return Ok(UseOutcome::NotConnectConfig);
        };

        let identity = UseIdentity {
            user_id: row.user_id,
            token_id: row.token_id,
            credential_hash_prefix: row.credential_hash_prefix,
            expires_at: row.expires_at,
        };

        if row.was_first_use {
            m::record_connect_config_consume("first_use");
            tracing::info!(
                target: "audit",
                action = "connect_config.consume",
                source = "api",
                client_ip = %ctx.client_ip,
                user_id = %identity.user_id,
                request_id = ?ctx.request_id,
                request_path = %ctx.request_path,
                token_id = %identity.token_id,
                credential_hash_prefix = %identity.credential_hash_prefix,
            );
            Ok(UseOutcome::FirstUse(identity))
        } else {
            m::record_connect_config_consume("repeat_use");
            Ok(UseOutcome::RepeatUse(identity))
        }
    }
}

fn source_str(source: &IssueSource) -> &'static str {
    match source {
        IssueSource::Http { .. } => "api",
        IssueSource::Cli { .. } => "cli",
    }
}

fn emit_token_issued(
    req: &IssueRequest,
    token_id: &str,
    credential_hash_prefix: &str,
    expires_at: &str,
) {
    let request_id = match &req.source {
        IssueSource::Http { request_id, .. } => request_id.clone(),
        IssueSource::Cli { .. } => None,
    };
    tracing::info!(
        target: "boundary",
        event = "connect_config.token_issued",
        component = "cmdock/server",
        correlation_id = %token_id,
        credential_hash_prefix = %credential_hash_prefix,
        source = %source_str(&req.source),
        user_id = %req.user_id,
        expires_at = %expires_at,
        request_id = ?request_id,
    );
}

fn emit_generate_audit(
    req: &IssueRequest,
    token_id: &str,
    credential_hash_prefix: &str,
    expires_at: &str,
    connect_url: Option<&str>,
) {
    match &req.source {
        IssueSource::Http {
            client_ip,
            request_id,
        } => {
            tracing::info!(
                target: "audit",
                action = "connect_config.generate",
                source = "api",
                client_ip = %client_ip,
                user_id = %req.user_id,
                token_id = %token_id,
                credential_hash_prefix = %credential_hash_prefix,
                expires_at = %expires_at,
                request_id = ?request_id,
                name = ?req.display_name,
            );
        }
        IssueSource::Cli { .. } => {
            let url_length = connect_url.map(str::len).unwrap_or(0);
            tracing::info!(
                target: "audit",
                action = "connect_config.generate",
                source = "cli",
                client_ip = "local",
                user_id = %req.user_id,
                token_id = %token_id,
                credential_hash_prefix = %credential_hash_prefix,
                expires_at = %expires_at,
                url_length = url_length,
            );
        }
    }
}

/// Resolve a connect-config server URL from an explicit override or the
/// server's `[server].public_base_url` config. Re-exported so the CLI
/// adapter can share the precedence rule with the HTTP path (which gets
/// `public_base_url` directly off the request state).
pub fn resolve_server_url(
    config_public_base_url: Option<&str>,
    override_url: Option<&str>,
) -> anyhow::Result<String> {
    match override_url {
        Some(url) => normalize_connect_server_url(url),
        None => {
            let raw = config_public_base_url.ok_or_else(|| {
                anyhow::anyhow!(
                    "connect-config server URL is not configured; set [server].public_base_url or pass --server-url"
                )
            })?;
            normalize_connect_server_url(raw)
        }
    }
}

/// Convenience helper for HTTP audit `client_ip` extraction. Centralised
/// so the HTTP adapter doesn't import `crate::audit` directly.
pub fn http_client_ip(headers: &axum::http::HeaderMap, trust_forwarded_headers: bool) -> String {
    audit::client_ip(headers, trust_forwarded_headers)
}

/// Convenience helper for HTTP audit `request_id` extraction.
pub fn http_request_id(headers: &axum::http::HeaderMap) -> Option<String> {
    audit::request_id(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_server_url_prefers_explicit_override() {
        let resolved = resolve_server_url(
            Some("https://tasks.example.com"),
            Some("https://override.example.com"),
        )
        .unwrap();
        assert_eq!(resolved, "https://override.example.com");
    }

    #[test]
    fn resolve_server_url_uses_config_default() {
        let resolved = resolve_server_url(Some("https://tasks.example.com/"), None).unwrap();
        assert_eq!(resolved, "https://tasks.example.com");
    }

    #[test]
    fn resolve_server_url_requires_https_origin() {
        let err = resolve_server_url(Some("http://tasks.example.com"), None).unwrap_err();
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn resolve_server_url_errors_when_unset() {
        let err = resolve_server_url(None, None).unwrap_err();
        assert!(err.to_string().contains("public_base_url"));
    }
}
