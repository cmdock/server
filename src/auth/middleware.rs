use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use lru::LruCache;
use std::num::NonZeroUsize;
use tokio::sync::Mutex;

use crate::app_state::AppState;
use crate::audit;
use crate::auth::runtime_access::enforce_runtime_access;
use crate::metrics as m;
use crate::store::models::{ConnectConfigTokenCorrelation, ConnectConfigTokenUse};

/// Extracted from the Authorization header. Available in handlers as an extractor.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: String,
    pub username: String,
    pub created_at: String,
}

/// Minimal auth identity — only what handlers need (no password_hash).
#[derive(Clone)]
struct CachedIdentity {
    user_id: String,
    username: String,
    created_at: String,
    connect_config: Option<CachedConnectConfig>,
    cached_at: Instant,
}

#[derive(Clone)]
struct CachedConnectConfig {
    token_id: String,
    credential_hash_prefix: String,
}

/// Cached token-to-user mapping with TTL.
///
/// Caches only runtime identity fields (not password_hash or other sensitive fields).
/// TTL is short (30s) to limit the window where revoked tokens remain valid.
#[derive(Clone)]
pub struct AuthCache {
    cache: Arc<Mutex<LruCache<String, CachedIdentity>>>,
}

const AUTH_CACHE_TTL: Duration = Duration::from_secs(30);
const AUTH_CACHE_SIZE: usize = 1024;

impl Default for AuthCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(AUTH_CACHE_SIZE).unwrap(),
            ))),
        }
    }

    async fn get(&self, token: &str) -> Option<CachedIdentity> {
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.get(token) {
            if entry.cached_at.elapsed() < AUTH_CACHE_TTL {
                m::record_auth_cache("hit");
                return Some(entry.clone());
            }
            cache.pop(token);
        }
        m::record_auth_cache("miss");
        None
    }

    async fn put(
        &self,
        token: String,
        user_id: String,
        username: String,
        created_at: String,
        connect_config: Option<CachedConnectConfig>,
    ) {
        let mut cache = self.cache.lock().await;
        cache.put(
            token,
            CachedIdentity {
                user_id,
                username,
                created_at,
                connect_config,
                cached_at: Instant::now(),
            },
        );
    }
}

fn log_connect_config_token_rejected(
    headers: &axum::http::HeaderMap,
    state: &AppState,
    path: &str,
    correlation: &ConnectConfigTokenCorrelation,
    reason: &str,
) {
    tracing::error!(
        target: "boundary",
        event = "connect_config.token_exchange_rejected",
        component = "cmdock/server",
        correlation_id = %correlation.token_id,
        credential_hash_prefix = %correlation.credential_hash_prefix,
        request_id = ?audit::request_id(headers),
        user_id = %correlation.user_id,
        request_path = %path,
        client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
        reason = %reason,
    );
}

fn log_connect_config_token_redeemed(
    headers: &axum::http::HeaderMap,
    state: &AppState,
    path: &str,
    correlation: &ConnectConfigTokenCorrelation,
) {
    tracing::info!(
        target: "boundary",
        event = "connect_config.token_redeemed",
        component = "cmdock/server",
        correlation_id = %correlation.token_id,
        credential_hash_prefix = %correlation.credential_hash_prefix,
        request_id = ?audit::request_id(headers),
        user_id = %correlation.user_id,
        request_path = %path,
        client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
    );
}

fn log_connection_established(
    headers: &axum::http::HeaderMap,
    state: &AppState,
    path: &str,
    correlation: &ConnectConfigTokenCorrelation,
) {
    tracing::info!(
        target: "boundary",
        event = "connection.established",
        component = "cmdock/server",
        correlation_id = %correlation.token_id,
        credential_hash_prefix = %correlation.credential_hash_prefix,
        request_id = ?audit::request_id(headers),
        user_id = %correlation.user_id,
        request_path = %path,
        client_ip = %audit::client_ip(headers, state.config.server.trust_forwarded_headers),
    );
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let request_path = parts.uri.path().to_string();
        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                let client_ip = audit::client_ip(
                    &parts.headers,
                    state.config.server.trust_forwarded_headers,
                );
                tracing::warn!(target: "audit", action = "auth.failure", source = "api", client_ip = %client_ip, request_id = ?audit::request_id(&parts.headers), request_path = %request_path, reason = "missing_authorization_header");
                (StatusCode::UNAUTHORIZED, "Missing Authorization header")
            })?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or_else(|| {
                let client_ip = audit::client_ip(
                    &parts.headers,
                    state.config.server.trust_forwarded_headers,
                );
                tracing::warn!(target: "audit", action = "auth.failure", source = "api", client_ip = %client_ip, request_id = ?audit::request_id(&parts.headers), request_path = %request_path, reason = "invalid_authorization_format");
                (StatusCode::UNAUTHORIZED, "Invalid Authorization format")
            })?;

        // Try cache first
        if let Some(entry) = state.auth_cache.get(token).await {
            let auth_user = AuthUser {
                user_id: entry.user_id.clone(),
                username: entry.username.clone(),
                created_at: entry.created_at.clone(),
            };
            if let Err(rejection) = enforce_runtime_access(
                state.store.clone(),
                &parts.headers,
                &auth_user.user_id,
                None,
                state.config.server.trust_forwarded_headers,
            )
            .await
            {
                if let Some(connect_config) = entry.connect_config.as_ref() {
                    let correlation = ConnectConfigTokenCorrelation {
                        user_id: auth_user.user_id.clone(),
                        token_id: connect_config.token_id.clone(),
                        credential_hash_prefix: connect_config.credential_hash_prefix.clone(),
                        expires_at: None,
                        is_expired: false,
                    };
                    log_connect_config_token_rejected(
                        &parts.headers,
                        state,
                        &request_path,
                        &correlation,
                        rejection.message,
                    );
                }
                return Err((rejection.status, rejection.message));
            }
            return Ok(auth_user);
        }

        // Cache miss — query the store
        let start = Instant::now();
        let user = state
            .store
            .get_user_by_token(token)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error"))?;

        let connect_config = state
            .store
            .lookup_connect_config_token(token)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error"))?;

        let user = match user {
            Some(user) => user,
            None => {
                if let Some(correlation) = connect_config.as_ref() {
                    let reason = if correlation.is_expired {
                        "expired_token"
                    } else {
                        "invalid_or_revoked_token"
                    };
                    log_connect_config_token_rejected(
                        &parts.headers,
                        state,
                        &request_path,
                        correlation,
                        reason,
                    );
                }
                let client_ip =
                    audit::client_ip(&parts.headers, state.config.server.trust_forwarded_headers);
                tracing::warn!(target: "audit", action = "auth.failure", source = "api", client_ip = %client_ip, request_id = ?audit::request_id(&parts.headers), request_path = %request_path, reason = "invalid_token");
                return Err((StatusCode::UNAUTHORIZED, "Invalid token"));
            }
        };

        m::record_config_db_op("auth_check", start.elapsed().as_secs_f64());

        let auth_user = AuthUser {
            user_id: user.id.clone(),
            username: user.username.clone(),
            created_at: user.created_at.clone(),
        };

        if let Err(rejection) = enforce_runtime_access(
            state.store.clone(),
            &parts.headers,
            &auth_user.user_id,
            None,
            state.config.server.trust_forwarded_headers,
        )
        .await
        {
            if let Some(correlation) = connect_config.as_ref() {
                log_connect_config_token_rejected(
                    &parts.headers,
                    state,
                    &request_path,
                    correlation,
                    rejection.message,
                );
            }
            return Err((rejection.status, rejection.message));
        }

        let client_ip =
            audit::client_ip(&parts.headers, state.config.server.trust_forwarded_headers);
        match state
            .store
            .record_connect_config_token_use(token, &client_ip)
            .await
        {
            Ok(ConnectConfigTokenUse::FirstUse(correlation)) => {
                m::record_connect_config_consume("first_use");
                log_connect_config_token_redeemed(
                    &parts.headers,
                    state,
                    &request_path,
                    &correlation,
                );
                log_connection_established(&parts.headers, state, &request_path, &correlation);
                tracing::info!(
                    target: "audit",
                    action = "connect_config.consume",
                    source = "api",
                    client_ip = %client_ip,
                    user_id = %auth_user.user_id,
                    request_id = ?audit::request_id(&parts.headers),
                    request_path = %request_path,
                    token_id = %correlation.token_id,
                    credential_hash_prefix = %correlation.credential_hash_prefix,
                );
            }
            Ok(ConnectConfigTokenUse::RepeatUse(_)) => {
                m::record_connect_config_consume("repeat_use");
            }
            Ok(ConnectConfigTokenUse::NotConnectConfig) => {}
            Err(err) => {
                tracing::warn!("Failed to record connect-config token use: {err}");
            }
        }

        // Cache only runtime identity fields (not password_hash)
        state
            .auth_cache
            .put(
                token.to_string(),
                user.id,
                user.username,
                user.created_at,
                connect_config.map(|correlation| CachedConnectConfig {
                    token_id: correlation.token_id,
                    credential_hash_prefix: correlation.credential_hash_prefix,
                }),
            )
            .await;

        Ok(auth_user)
    }
}
