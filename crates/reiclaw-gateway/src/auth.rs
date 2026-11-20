//! Shared token parsing and paired-request admission.
//!
//! Parsing never grants authority. Operator, admin, bridge, and Node callers
//! retain their own credential/identity policies after extracting a token.

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Json, Response},
};

use crate::AppState;

/// Preserve the existing, case-sensitive HTTP bearer syntax. An empty bearer
/// remains distinguishable from a missing/malformed header for callers.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// WebSocket transports share header -> subprotocol -> query precedence.
/// Empty candidates fall through, and token bytes are never trimmed.
pub(crate) fn ws_bearer_token<'a>(
    headers: &'a HeaderMap,
    query_token: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(token) = bearer_token(headers).filter(|token| !token.is_empty()) {
        return Some(token);
    }
    let protocol_token = headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .and_then(|protocols| {
            protocols
                .split(',')
                .map(str::trim)
                .find_map(|protocol| protocol.strip_prefix("bearer."))
        })
        .filter(|token| !token.is_empty());
    protocol_token.or_else(|| query_token.filter(|token| !token.is_empty()))
}

pub(crate) fn paired_authenticates(state: &AppState, headers: &HeaderMap) -> bool {
    !state.pairing.require_pairing()
        || bearer_token(headers)
            .filter(|token| !token.is_empty())
            .is_some_and(|token| state.pairing.is_authenticated(token))
}

pub(crate) fn require_paired(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if paired_authenticates(state, headers) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "Unauthorized — pair first via POST /pair, then send Authorization: Bearer <token>"
            })),
        ))
    }
}

/// Admission for ordinary paired APIs. Attaching it to the handler keeps the
/// gate active on every router mounting that handler, without covering public
/// OPTIONS, pairing, or transport-specific identity routes accidentally.
pub struct PairedAuth(());

impl FromRequestParts<AppState> for PairedAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        require_paired(state, &parts.headers).map_err(IntoResponse::into_response)?;
        Ok(Self(()))
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
