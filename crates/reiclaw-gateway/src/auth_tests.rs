use super::*;
use axum::{
    Router,
    body::Body,
    http::{HeaderValue, Method, Request},
    routing::{get, post},
};
use reiclaw_runtime::security::pairing::PairingGuard;
use std::sync::Arc;
use tower::ServiceExt;

#[test]
fn http_bearer_keeps_existing_syntax_and_token_bytes() {
    let mut headers = HeaderMap::new();
    assert_eq!(bearer_token(&headers), None);
    for (value, expected) in [
        ("Bearer paired", Some("paired")),
        ("Bearer ", Some("")),
        ("Bearer  paired ", Some(" paired ")),
        ("bearer paired", None),
        ("BEARER paired", None),
        ("Basic paired", None),
        ("Bearer", None),
    ] {
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        assert_eq!(bearer_token(&headers), expected, "{value:?}");
    }
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
    );
    assert_eq!(bearer_token(&headers), None);
}

#[test]
fn ws_bearer_keeps_precedence_and_empty_candidate_fallback() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer header-token".parse().unwrap(),
    );
    headers.insert(
        "sec-websocket-protocol",
        "reiclaw.v1, bearer.protocol-token".parse().unwrap(),
    );
    assert_eq!(
        ws_bearer_token(&headers, Some("query-token")),
        Some("header-token")
    );
    headers.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
    assert_eq!(
        ws_bearer_token(&headers, Some("query-token")),
        Some("protocol-token")
    );
    // Preserve first matching subprotocol semantics: an empty first bearer
    // falls back to the query, rather than selecting a later credential.
    headers.insert(
        "sec-websocket-protocol",
        "bearer., bearer.later-token".parse().unwrap(),
    );
    assert_eq!(
        ws_bearer_token(&headers, Some("query-token")),
        Some("query-token")
    );
    assert_eq!(ws_bearer_token(&headers, Some("")), None);
    headers.insert(header::AUTHORIZATION, "Bearer  spaced ".parse().unwrap());
    assert_eq!(
        ws_bearer_token(&headers, Some("query-token")),
        Some(" spaced ")
    );
}

fn state(require_pairing: bool) -> AppState {
    let mut config = reiclaw_config::schema::Config::default();
    config.gateway.operator.token_hashes = vec![PairingGuard::token_hash("operator")];
    config.gateway.bridges.insert(
        "fixture".into(),
        reiclaw_config::schema::GatewayBridgeConfig {
            token_hash: PairingGuard::token_hash("bridge"),
            session_prefix: Some("fixture:".into()),
            sessions: Vec::new(),
            owner: None,
            device: None,
        },
    );
    config.nodes.auth_token = Some("node".into());
    let mut state = crate::api::test_state(config);
    state.pairing = Arc::new(PairingGuard::new(
        require_pairing,
        &[PairingGuard::token_hash("paired")],
    ));
    state
}

fn paired_routes(state: AppState) -> Router {
    use crate::{api_logs, api_personality, api_sections, api_skills, sse};
    Router::new()
        .route("/api/logs", get(api_logs::handle_api_logs))
        .route("/api/personality", get(api_personality::handle_index))
        .route(
            "/api/personality/templates",
            get(api_personality::handle_templates),
        )
        .route(
            "/api/personality/{filename}",
            get(api_personality::handle_get).put(api_personality::handle_put),
        )
        .route(
            "/api/skills/slash-option-kinds",
            get(api_skills::handle_slash_option_kinds),
        )
        .route("/api/skills/bundles", get(api_skills::handle_list_bundles))
        .route(
            "/api/agents/{alias}/skills",
            get(api_skills::handle_agent_skills),
        )
        .route(
            "/api/skills/bundles/{alias}/skills",
            get(api_skills::handle_list_skills).post(api_skills::handle_create_skill),
        )
        .route(
            "/api/skills/bundles/{alias}/skills/{name}",
            get(api_skills::handle_read_skill)
                .put(api_skills::handle_write_skill)
                .delete(api_skills::handle_delete_skill),
        )
        .route("/api/config/catalog", get(api_sections::handle_catalog))
        .route(
            "/api/config/catalog/models",
            get(api_sections::handle_catalog_models),
        )
        .route(
            "/api/config/status",
            get(api_sections::handle_section_status),
        )
        .route(
            "/api/config/agent-options",
            get(api_sections::handle_agent_options),
        )
        .route("/api/config/sections", get(api_sections::handle_sections))
        .route(
            "/api/config/sections/{section}",
            get(api_sections::handle_section_picker),
        )
        .route("/api/events/history", get(sse::handle_events_history))
        .with_state(state)
}

fn request(method: Method, path: &str, bearer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from("{}")).unwrap()
}

#[tokio::test]
async fn paired_extractors_cover_every_migrated_handler() {
    let operations = [
        (Method::GET, "/api/logs"),
        (Method::GET, "/api/personality"),
        (Method::GET, "/api/personality/templates"),
        (Method::GET, "/api/personality/USER.md"),
        (Method::PUT, "/api/personality/USER.md"),
        (Method::GET, "/api/skills/slash-option-kinds"),
        (Method::GET, "/api/skills/bundles"),
        (Method::GET, "/api/agents/missing/skills"),
        (Method::GET, "/api/skills/bundles/missing/skills"),
        (Method::POST, "/api/skills/bundles/missing/skills"),
        (Method::GET, "/api/skills/bundles/missing/skills/missing"),
        (Method::PUT, "/api/skills/bundles/missing/skills/missing"),
        (Method::DELETE, "/api/skills/bundles/missing/skills/missing"),
        (Method::GET, "/api/config/catalog"),
        (Method::GET, "/api/config/catalog/models?provider=missing"),
        (Method::GET, "/api/config/status"),
        (Method::GET, "/api/config/agent-options"),
        (Method::GET, "/api/config/sections"),
        (Method::GET, "/api/config/sections/missing"),
        (Method::GET, "/api/events/history"),
    ];
    let authenticated = paired_routes(state(true));
    let pairing_off = paired_routes(state(false));
    for (method, path) in operations {
        for token in [
            None,
            Some(""),
            Some("wrong"),
            Some("operator"),
            Some("bridge"),
            Some("node"),
        ] {
            let response = authenticated
                .clone()
                .oneshot(request(method.clone(), path, token))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}, {token:?}"
            );
        }
        let paired = authenticated
            .clone()
            .oneshot(request(method.clone(), path, Some("paired")))
            .await
            .unwrap();
        let anonymous = pairing_off
            .clone()
            .oneshot(request(method.clone(), path, None))
            .await
            .unwrap();
        assert_ne!(paired.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        assert_eq!(
            paired.status(),
            anonymous.status(),
            "pairing-off behavior changed: {method} {path}"
        );
    }
}

#[tokio::test]
async fn config_method_retirement_preserves_public_options_and_paired_patch() {
    let app = crate::api_config::routes().with_state(state(true));
    assert_eq!(
        app.clone()
            .oneshot(request(Method::GET, "/api/config", Some("paired")))
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED,
    );
    let mut patch = request(Method::PATCH, "/api/config", None);
    patch
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            4321,
        ))));
    assert_eq!(
        app.clone().oneshot(patch).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let schema = app
        .clone()
        .oneshot(request(Method::OPTIONS, "/api/config", None))
        .await
        .unwrap();
    assert_eq!(schema.status(), StatusCode::OK);
    assert_eq!(schema.headers()[header::ALLOW], "PATCH, OPTIONS");
    for (path, method, allowed) in [
        ("/api/config", "PATCH", "PATCH, OPTIONS"),
        (
            "/api/config/prop?path=gateway.port",
            "PUT",
            "GET, PUT, DELETE, OPTIONS",
        ),
    ] {
        let mut preflight = request(Method::OPTIONS, path, None);
        preflight
            .headers_mut()
            .insert("access-control-request-method", method.parse().unwrap());
        let response = app.clone().oneshot(preflight).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
        assert_eq!(response.headers()["access-control-allow-methods"], allowed);
        assert!(
            response.headers()["access-control-allow-headers"]
                .to_str()
                .unwrap()
                .contains("Authorization")
        );
    }
}

#[tokio::test]
async fn operator_extractor_keeps_the_shared_rate_limit_and_single_admission() {
    use crate::operator_auth::{OperatorAuth, gate_operator_identity};
    async fn admitted(_: OperatorAuth) -> StatusCode {
        StatusCode::NO_CONTENT
    }
    let peer = std::net::SocketAddr::from(([127, 0, 0, 2], 3210));
    for wrong_attempts in [true, false] {
        let reference = state(false);
        let actual = state(false);
        // A small pairing ceiling catches accidentally gating twice. The
        // auth-limiter constants remain the production values.
        let mut reference = reference;
        let mut actual = actual;
        reference.rate_limiter = Arc::new(crate::GatewayRateLimiter::new(3, 100, 100));
        actual.rate_limiter = Arc::new(crate::GatewayRateLimiter::new(3, 100, 100));
        let app = Router::new()
            .route("/operator", post(admitted))
            .with_state(actual);
        let attempts = if wrong_attempts {
            crate::auth_rate_limit::MAX_ATTEMPTS + 2
        } else {
            5
        };
        let mut saw_limit = false;
        for _ in 0..attempts {
            let token = if wrong_attempts { "paired" } else { "operator" };
            let mut req = request(Method::POST, "/operator", Some(token));
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
            let expected = gate_operator_identity(&reference, peer, req.headers())
                .map_or(StatusCode::NO_CONTENT, |response| response.status());
            let response = app.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), expected);
            saw_limit |= expected == StatusCode::TOO_MANY_REQUESTS;
        }
        assert!(saw_limit, "the limiter must actually be exercised");
    }
}
