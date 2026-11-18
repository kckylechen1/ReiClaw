async fn consent_http(
    state: &AppState,
    head: &str,
    method: &str,
    token: Option<&str>,
    payload: &str,
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let app = voice::routes()
        .with_state(state.clone())
        .layer(axum::Extension(ConnectInfo(loopback_peer())));
    let mut request = axum::http::Request::builder().method(method).uri(format!(
        "/api/user-model/heads/{head}/voice-consent?provider=moss-live"
    ));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    json_of(
        app.oneshot(
            request
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
}

#[tokio::test]
async fn voice_consent_legacy_paired_token_cannot_read_grant_or_revoke() {
    let (dir, state) = state_with_tempdir();
    assert!(!state.config.read().gateway.operator.is_configured());
    let store = UserModelStore::open(dir.path()).unwrap();
    let head = store
        .record_owner_statement(
            UserModelKind::Preference,
            "Synthetic private fact.",
            "style",
            "global",
            1,
        )
        .unwrap();
    // Exercise grant with no receipt and revoke with a real grant to preserve.
    for (action, revision) in [("grant", 0), ("revoke", 1)] {
        let before = store.voice_consent(&head.id, "moss-live").unwrap();
        let payload = serde_json::json!({"provider":"moss-live", "action":action, "expected_consent_revision":revision}).to_string();
        for token in [Some("op-token"), None] {
            for method in ["GET", "PUT"] {
                let (status, response) =
                    consent_http(&state, &head.id, method, token, &payload).await;
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert_eq!(response["code"], "VOICE_CONSENT_OPERATOR_CONFIG_REQUIRED");
                assert!(
                    response["message"]
                        .as_str()
                        .unwrap()
                        .contains("gateway.operator.token_hashes")
                );
                assert!(response.get("receipt").is_none());
                assert_eq!(store.voice_consent(&head.id, "moss-live").unwrap(), before);
            }
        }
        if revision == 0 {
            store
                .set_voice_consent(
                    &head.id,
                    "moss-live",
                    reiclaw_memory::companion::VoiceConsentAction::Grant,
                    0,
                    2,
                )
                .unwrap();
        }
    }
    assert_eq!(store.active_heads(None).unwrap(), [head]);
}

#[tokio::test]
async fn voice_consent_routes_gate_before_parsing_and_persist_exact_decisions() {
    let (dir, state) = state_with_tempdir();
    state.config.write().gateway.operator.token_hashes =
        vec![reiclaw_config::tokens::hash_token("synthetic-operator")];
    // Pairing-off and the legacy paired device still cannot become operator.
    for paired in [true, false] {
        let mut state = state.clone();
        state.auth_limiter = Arc::new(crate::auth_rate_limit::AuthRateLimiter::new());
        state.pairing = Arc::new(reiclaw_runtime::security::pairing::PairingGuard::new(
            paired,
            &["op-token".into()],
        ));
        for token in [None, Some("op-token"), Some("synthetic-bridge")] {
            for method in ["GET", "PUT"] {
                let (status, _) = consent_http(&state, "absent", method, token, "not-json").await;
                assert!(matches!(
                    status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ));
                assert!(!dir.path().join("user_model.db").exists());
            }
        }
    }
    let store = UserModelStore::open(dir.path()).unwrap();
    let head = store
        .record_owner_statement(
            UserModelKind::Preference,
            "Prefers concise replies.",
            "style",
            "global",
            1,
        )
        .unwrap();
    let (status, response) =
        consent_http(&state, &head.id, "GET", Some("synthetic-operator"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["receipt"], serde_json::Value::Null);
    for (action, expected) in [("grant", 0), ("revoke", 1)] {
        let payload = serde_json::json!({"provider":"moss-live", "action":action, "expected_consent_revision":expected}).to_string();
        let (status, response) = consent_http(
            &state,
            &head.id,
            "PUT",
            Some("synthetic-operator"),
            &payload,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["receipt"]["action"], action);
        assert_eq!(response["receipt"]["head_revision_id"], head.id);
        assert_eq!(response["receipt"]["reviewer"], "owner_operator");
        assert_eq!(response["receipt"]["revision"], expected + 1);
        assert_eq!(
            consent_http(&state, &head.id, "GET", Some("synthetic-operator"), "")
                .await
                .1,
            response
        );
    }
    let stale = r#"{"provider":"moss-live","action":"grant","expected_consent_revision":1}"#;
    assert_eq!(
        consent_http(&state, &head.id, "PUT", Some("synthetic-operator"), stale)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let spoof = r#"{"provider":"moss-live","action":"grant","expected_consent_revision":2,"reviewer":"owner"}"#;
    assert_eq!(
        consent_http(&state, &head.id, "PUT", Some("synthetic-operator"), spoof)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    drop(store);
    let reopened = UserModelStore::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .voice_consent(&head.id, "moss-live")
            .unwrap()
            .unwrap()
            .action,
        reiclaw_memory::companion::VoiceConsentAction::Revoke
    );
    assert_eq!(reopened.active_heads(None).unwrap(), [head]);
}
