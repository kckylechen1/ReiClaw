//! Feature-gated owner consent; no bridge or model write surface.
use super::*;
use reiclaw_api::companion::VoiceConsentAction;
use reiclaw_memory::companion::validate_voice_provider;

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/api/user-model/heads/{id}/voice-consent",
        axum::routing::get(get).put(put),
    )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentBody {
    provider: String,
    action: VoiceConsentAction,
    expected_consent_revision: u64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderQuery {
    provider: String,
}

fn refusal(status: StatusCode, code: &str) -> Response {
    (status, axum::Json(serde_json::json!({"code": code}))).into_response()
}

fn operator_configuration_required() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "code": "VOICE_CONSENT_OPERATOR_CONFIG_REQUIRED",
            "message": reiclaw_runtime::i18n::get_required_cli_string("gateway-voice-consent-operator-config-required"),
        })),
    )
        .into_response()
}

fn gate_voice_operator(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Option<Response> {
    if !state.config.read().gateway.operator.is_configured() {
        return Some(operator_configuration_required());
    }
    if let Some(error) = crate::operator_auth::gate_operator_identity(state, peer, headers) {
        return Some(error);
    }
    // A reload must not turn the shared gate's legacy fallback into voice consent.
    let config = state.config.read();
    if !config.gateway.operator.is_configured() {
        return Some(operator_configuration_required());
    }
    let token = crate::operator_auth::extract_bearer(headers).unwrap_or("");
    if !config.gateway.operator.authenticates(token) {
        return Some(refusal(
            StatusCode::FORBIDDEN,
            "VOICE_CONSENT_OPERATOR_REQUIRED",
        ));
    }
    None
}

async fn get(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(head): Path<String>,
    uri: Uri,
) -> Response {
    if let Some(err) = gate_voice_operator(&state, peer, &headers) {
        return err;
    }
    let Ok(query) = axum::extract::Query::<ProviderQuery>::try_from_uri(&uri) else {
        return refusal(StatusCode::BAD_REQUEST, "INVALID_VOICE_CONSENT");
    };
    if validate_voice_provider(&query.provider).is_err() {
        return refusal(StatusCode::BAD_REQUEST, "INVALID_VOICE_PROVIDER");
    }
    let data_dir = data_dir_of(&state);
    match tokio::task::spawn_blocking(move || {
        cached_store(&data_dir)?
            .voice_consent(&head, &query.provider)
            .map_err(|e| e.to_string())
    })
    .await
    {
        Ok(Ok(receipt)) => axum::Json(serde_json::json!({"receipt": receipt})).into_response(),
        _ => refusal(StatusCode::SERVICE_UNAVAILABLE, "VOICE_CONSENT_UNAVAILABLE"),
    }
}

async fn put(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(head): Path<String>,
    body: Bytes,
) -> Response {
    if let Some(err) = gate_voice_operator(&state, peer, &headers) {
        return err;
    }
    let Ok(body) = serde_json::from_slice::<ConsentBody>(&body) else {
        return refusal(StatusCode::BAD_REQUEST, "INVALID_VOICE_CONSENT");
    };
    if validate_voice_provider(&body.provider).is_err() {
        return refusal(StatusCode::BAD_REQUEST, "INVALID_VOICE_PROVIDER");
    }
    let data_dir = data_dir_of(&state);
    let result = tokio::task::spawn_blocking(move || {
        let store = cached_store(&data_dir).map_err(ReviewStoreError::Open)?;
        store
            .set_voice_consent(
                &head,
                &body.provider,
                body.action,
                body.expected_consent_revision,
                now_unix(),
            )
            .map_err(ReviewStoreError::Review)
    })
    .await;
    match result {
        Ok(Ok(receipt)) => axum::Json(serde_json::json!({"receipt": receipt})).into_response(),
        Ok(Err(ReviewStoreError::Review(ref error))) if is_user_model_head_conflict(error) => {
            refusal(StatusCode::CONFLICT, "VOICE_CONSENT_CONFLICT")
        }
        Ok(Err(ReviewStoreError::Review(rusqlite::Error::QueryReturnedNoRows))) => {
            refusal(StatusCode::NOT_FOUND, "VOICE_CONSENT_HEAD_NOT_FOUND")
        }
        _ => refusal(StatusCode::SERVICE_UNAVAILABLE, "VOICE_CONSENT_UNAVAILABLE"),
    }
}
