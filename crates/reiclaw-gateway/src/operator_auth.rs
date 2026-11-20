//! Shared operator-bearer gate for management API surfaces.
//!
//! Extracted from `api_node_identity` so non-nodes operator surfaces (the
//! User Model review API) enforce the identical contract: operator bearer
//! mandatory regardless of `require_pairing`, strict rate limiting on
//! presented-but-wrong tokens, and the pairing-rate ceiling after success.
//! A configured operator hash list is authoritative; only an empty list
//! retains legacy paired-token operator access during explicit migration.

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{HeaderMap, StatusCode, request::Parts};
use axum::response::{IntoResponse, Json};
use reiclaw_runtime::security::pairing::{PairingGuard, constant_time_eq};
use std::net::SocketAddr;

use crate::AppState;

/// Called at startup and when configuration deliberately restores legacy mode.
pub(crate) fn warn_legacy_operator(config: &reiclaw_config::schema::GatewayOperatorConfig) {
    if !config.is_configured() {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
            "No gateway.operator.token_hashes configured: paired tokens retain legacy operator authority. Configure independent operator token hashes to restrict management access."
        );
    }
}

pub(crate) use crate::auth::bearer_token as extract_bearer;

pub(crate) fn require_operator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    let token = extract_bearer(headers).unwrap_or("");
    if token.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    let config = state.config.read();
    if config.gateway.operator.is_configured() {
        if config.gateway.operator.authenticates(token) {
            return Ok(());
        }
        let hashed = PairingGuard::token_hash(token);
        return if state
            .pairing
            .tokens()
            .iter()
            .any(|stored| constant_time_eq(stored, &hashed))
        {
            Err((StatusCode::FORBIDDEN, "Forbidden"))
        } else {
            Err((StatusCode::UNAUTHORIZED, "Unauthorized"))
        };
    }
    let hashed = PairingGuard::token_hash(token);
    let known = state.pairing.tokens();
    if !known.iter().any(|stored| constant_time_eq(stored, &hashed)) {
        return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    Ok(())
}

pub(crate) fn gate_operator_identity(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Option<axum::response::Response> {
    let rate_key =
        crate::client_key_from_request(Some(peer), headers, state.trust_forwarded_headers);
    let presented = extract_bearer(headers).is_some_and(|token| !token.is_empty());
    if let Err(err) = require_operator(state, headers) {
        if presented {
            if let Err(e) = state.auth_limiter.check_rate_limit_strict(&rate_key) {
                let body = serde_json::json!({
                    "error": format!("Too many auth attempts. Try again in {}s.", e.retry_after_secs),
                    "retry_after": e.retry_after_secs,
                });
                return Some((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
            }
        }
        state.auth_limiter.record_attempt_strict(&rate_key);
        return Some(err.into_response());
    }
    if let Err(e) = state.auth_limiter.check_rate_limit_strict(&rate_key) {
        let body = serde_json::json!({
            "error": format!("Too many auth attempts. Try again in {}s.", e.retry_after_secs),
            "retry_after": e.retry_after_secs,
        });
        return Some((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
    }
    if !state.rate_limiter.allow_pair(&rate_key) {
        let err = serde_json::json!({
            "error": "Too many pairing requests. Please retry later.",
            "retry_after": crate::RATE_LIMIT_WINDOW_SECS,
        });
        return Some((StatusCode::TOO_MANY_REQUESTS, Json(err)).into_response());
    }
    None
}

/// Per-request operator admission, including the existing strict auth and
/// pairing rate limits. It does not infer authority from pairing-off mode.
pub struct OperatorAuth {
    headers: HeaderMap,
}

impl OperatorAuth {
    pub(crate) fn token(&self) -> &str {
        // gate_operator_identity rejected absent/empty credentials already.
        extract_bearer(&self.headers).unwrap_or("")
    }
}

impl FromRequestParts<AppState> for OperatorAuth {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ConnectInfo(peer) = ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        if let Some(error) = gate_operator_identity(state, peer, &parts.headers) {
            return Err(error);
        }
        Ok(Self {
            headers: parts.headers.clone(),
        })
    }
}

#[cfg(test)]
#[path = "operator_auth_tests.rs"]
mod tests;
