use super::*;

// Compile-time proof that both reqwest TLS-root features are enabled.
// `tls_built_in_webpki_certs` is gated on `rustls-tls-webpki-roots-no-provider`;
// `tls_built_in_native_certs` is gated on `rustls-tls-native-roots-no-provider`.
// If either feature were dropped, this test would fail to compile.
#[test]
fn provider_http_client_trusts_both_webpki_and_native_roots() {
    let _client = reqwest::Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .build()
        .expect("client builder should succeed with both root sets enabled");
}

#[test]
fn vision_override_applies_once_at_construction() {
    for (vision, expected) in [(None, true), (Some(false), false), (Some(true), true)] {
        let options = ModelProviderRuntimeOptions {
            vision,
            ..Default::default()
        };
        let provider =
            create_model_provider_inner(None, "openai", "default", None, None, &options).unwrap();
        assert_eq!(provider.supports_vision(), expected, "vision={vision:?}");
        assert_eq!(
            provider.capabilities().vision,
            expected,
            "vision={vision:?}"
        );
    }
}

#[test]
fn vision_config_field_maps_into_runtime_options() {
    use reiclaw_config::schema::{Config, ModelProviderConfig};
    let entry = ModelProviderConfig {
        vision: Some(false),
        ..Default::default()
    };
    let opts =
        model_provider_runtime_options_from_model_provider_entry(&Config::default(), Some(&entry));
    assert_eq!(opts.vision, Some(false));
}

#[test]
fn openai_responses_alias_honors_configured_vision_capability() {
    use reiclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig, WireApi};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "responses_vision".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                wire_api: Some(WireApi::Responses),
                vision: Some(false),
                ..Default::default()
            },
        },
    );
    let options = provider_runtime_options_for_alias(&config, "openai", "responses_vision");
    let provider = create_model_provider_for_alias(
        &config,
        "openai",
        "responses_vision",
        Some("sk-test"),
        &options,
    )
    .expect("configured OpenAI Responses alias builds");

    assert_eq!(provider.default_wire_api(), "responses");
    assert!(
        provider.capabilities().native_tool_calling,
        "the alias must use the Responses provider"
    );
    assert!(
        !provider.capabilities_for_model("gpt-4o").vision,
        "vision=false must override the Responses default through the production factory path"
    );
}

#[test]
fn options_for_bare_provider_ref_does_not_inherit_fallback_vision() {
    use reiclaw_config::schema::Config;
    // A bare family ref (no alias) must not inherit the fallback provider's
    // `vision` flag — otherwise a `-p llamacpp` override would carry the
    // agent provider's capability. Falls back to the family default.
    let fallback = ModelProviderRuntimeOptions {
        vision: Some(false),
        ..Default::default()
    };
    let resolved = options_for_provider_ref(&Config::default(), "openai", &fallback);
    assert_eq!(resolved.vision, None);
}

#[test]
fn provider_runtime_options_from_config_propagates_tls_ca_cert_path() {
    // Regression guard: tls_ca_cert_path on a ModelProviderConfig entry must
    // reach ModelProviderRuntimeOptions so apply_compat_options can call
    // with_tls_ca_cert_path on the provider. Same pattern as native_tools above.
    use reiclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    let mut config = reiclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "corp".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                tls_ca_cert_path: Some("/tmp/example-ca.pem".to_string()),
                ..Default::default()
            },
        },
    );

    let entry = config.providers.models.find("openai", "corp");
    let options = model_provider_runtime_options_from_model_provider_entry(&config, entry);
    assert_eq!(
        options.tls_ca_cert_path.as_deref(),
        Some("/tmp/example-ca.pem"),
        "tls_ca_cert_path must propagate from ModelProviderConfig to ModelProviderRuntimeOptions"
    );
}

#[test]
fn provider_runtime_options_from_config_propagates_provider_kind() {
    use reiclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    let mut config = reiclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                kind: Some("openai".to_string()),
                uri: Some("http://primary.example/v1".to_string()),
                ..Default::default()
            },
        },
    );

    let options = provider_runtime_options_for_alias(&config, "openai", "primary");
    assert_eq!(options.provider_kind.as_deref(), Some("openai"));
    assert_eq!(
        options.provider_api_url.as_deref(),
        Some("http://primary.example/v1")
    );
}

#[tokio::test]
async fn routed_alias_uses_call_model_instead_of_configured_pin() {
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use reiclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    type Capture = Arc<Mutex<Option<String>>>;

    async fn capture_chat_request(
        State(capture): State<Capture>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        *capture.lock().expect("capture lock poisoned") = Some(model);
        (
            StatusCode::OK,
            Json(json!({
                "output_text": "ok"
            })),
        )
    }

    let capture: Capture = Arc::new(Mutex::new(None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let app = Router::new()
        .route("/v1/responses", post(capture_chat_request))
        .with_state(capture.clone());
    let server = ::reiclaw_log::spawn!(async move {
        axum::serve(listener, app).await.expect("serve test server");
    });

    let mut config = reiclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                api_key: Some("sk-test".to_string()),
                uri: Some(format!("http://{addr}/v1")),
                model: Some("old-model".to_string()),
                ..Default::default()
            },
        },
    );
    let provider = create_routed_model_provider_with_options(
        &config,
        "openai.primary",
        Some("sk-test"),
        Some(&format!("http://{addr}/v1")),
        &config.reliability,
        &[],
        "new-model",
        &ModelProviderRuntimeOptions::default(),
    )
    .expect("provider should build");
    let messages = vec![ChatMessage::user("hello")];
    let request = ChatRequest {
        messages: &messages,
        tools: None,
        thinking: None,
    };

    let response = provider
        .chat(request, "new-model", None)
        .await
        .expect("chat should succeed");

    assert_eq!(response.text.as_deref(), Some("ok"));
    let model = capture
        .lock()
        .expect("capture lock poisoned")
        .take()
        .expect("server should capture request");
    assert_eq!(model, "new-model");
    server.abort();
}

#[test]
fn route_provider_options_clear_primary_only_state_for_bare_routes() {
    let inherited = ModelProviderRuntimeOptions {
        provider_kind: Some("openai".to_string()),
        provider_api_url: Some("http://primary.example/v1".to_string()),
        ..Default::default()
    };
    let config = reiclaw_config::schema::Config::default();

    let route_options = options_for_provider_ref(&config, "openai", &inherited);

    assert_eq!(route_options.provider_kind, None);
    assert_eq!(route_options.provider_api_url, None);
}

#[test]
fn routed_bare_provider_does_not_inherit_primary_endpoint() {
    use reiclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    let mut config = reiclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                kind: Some("openai".to_string()),
                uri: Some("http://primary.example/v1".to_string()),
                ..Default::default()
            },
        },
    );
    let options = provider_runtime_options_for_alias(&config, "openai", "primary");
    assert_eq!(
        options.provider_api_url.as_deref(),
        Some("http://primary.example/v1")
    );

    let route_options = options_for_provider_ref(&config, "openai", &options);

    assert_eq!(route_options.provider_kind, None);
    assert_eq!(route_options.provider_api_url, None);
}

#[test]
fn routed_primary_alias_kind_does_not_leak_to_canonical_route_provider() {
    use reiclaw_config::schema::{
        ModelProviderConfig, ModelRouteConfig, OpenAIModelProviderConfig,
    };

    let mut config = reiclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                kind: Some("openai".to_string()),
                vision: Some(true),
                uri: Some("http://primary.example/v1".to_string()),
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "route".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig::default(),
        },
    );
    let options = provider_runtime_options_for_alias(&config, "openai", "primary");
    assert_eq!(options.provider_kind.as_deref(), Some("openai"));

    let provider = create_routed_model_provider_with_options(
        &config,
        "openai.primary",
        Some("sk-test"),
        None,
        &config.reliability,
        &[ModelRouteConfig {
            hint: "fast".to_string(),
            model_provider: "openai.route".to_string(),
            model: "claude-test".to_string(),
            api_key: None,
        }],
        "gpt-test",
        &options,
    )
    .expect("primary alias kind should build without poisoning route provider kind");

    assert!(
        provider.supports_vision(),
        "primary OpenAI Responses provider should remain the router default"
    );
}

#[test]
fn create_model_provider_from_ref_fails_closed_on_nonexistent_dotted_alias() {
    use reiclaw_config::schema::Config;
    // A dotted `vision_model_provider` that names no configured alias (e.g. a
    // typo) must fail CLOSED - an error the operator sees - not silently fall
    // open to a family-default provider. `llamacpp` builds a default endpoint
    // for a bare family, so without the entry-exists guard `llamacpp.typo`
    // would (wrongly) succeed and route images to a default llama.cpp.
    let config = Config::default();
    assert!(
        create_model_provider_from_ref(&config, "openai.typo").is_err(),
        "a dotted ref to a non-existent alias must fail closed, not fall open to a default provider"
    );
    // Same fail-closed behavior as the legacy factory the vision route used.
    assert!(create_model_provider("openai.typo", None).is_err());
}

#[test]
fn factory_unknown_provider_errors() {
    let p = create_model_provider("nonexistent", None);
    assert!(p.is_err());
    let msg = p.err().unwrap().to_string();
    assert!(msg.contains("Unknown model provider or alias"));
    assert!(msg.contains("[providers.models.openai.<alias>]"));
}

#[test]
fn factory_empty_name_errors() {
    assert!(create_model_provider("", None).is_err());
}

#[test]
fn listed_model_providers_have_unique_canonical_ids() {
    let model_providers = list_model_providers();
    let mut canonical_ids = std::collections::HashSet::new();

    for model_provider in model_providers {
        assert!(
            canonical_ids.insert(model_provider.name),
            "Duplicate canonical model model_provider id: {}",
            model_provider.name
        );
    }
}

#[test]
fn listed_model_providers_match_canonical_slots() {
    let listed: std::collections::BTreeSet<&str> =
        list_model_providers().iter().map(|p| p.name).collect();
    let canonical: std::collections::BTreeSet<&str> =
        canonical_model_provider_slots().into_iter().collect();
    let missing: Vec<&&str> = canonical.difference(&listed).collect();
    let phantom: Vec<&&str> = listed.difference(&canonical).collect();
    assert!(
        missing.is_empty() && phantom.is_empty(),
        "list_model_providers() drift — missing display entries: {missing:?}; \
             phantom entries (no factory slot): {phantom:?}"
    );
}

#[test]
fn listed_model_providers_are_constructible() {
    for model_provider in list_model_providers() {
        // The custom slot requires a uri (no family-default endpoint);
        // covered by dedicated factory tests.
        if model_provider.name == "custom" {
            continue;
        }
        assert!(
            create_model_provider(model_provider.name, Some("provider-test-credential")).is_ok(),
            "Canonical model model_provider id should be constructible: {}",
            model_provider.name
        );
    }
}

#[test]
fn format_error_chain_includes_sources_and_sanitizes_output() {
    #[derive(Debug)]
    struct ChainError {
        message: &'static str,
        source: Option<Box<dyn std::error::Error + 'static>>,
    }

    impl std::fmt::Display for ChainError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl std::error::Error for ChainError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref()
        }
    }

    let error = ChainError {
        message: "outer context",
        source: Some(Box::new(ChainError {
            message: "middle context",
            source: Some(Box::new(ChainError {
                message: "inner source leaked sk-1234567890abcdef",
                source: None,
            })),
        })),
    };

    let result = format_error_chain(&error);

    assert!(result.contains("outer context"));
    assert!(result.contains("middle context"));
    assert!(result.contains("inner source leaked [REDACTED]"));
    assert!(!result.contains("sk-1234567890abcdef"));
}

#[test]
fn sanitize_scrubs_sk_prefix() {
    let input = "request failed: sk-1234567890abcdef";
    let out = sanitize_api_error(input);
    assert!(!out.contains("sk-1234567890abcdef"));
    assert!(out.contains("[REDACTED]"));
}

#[test]
fn sanitize_scrubs_multiple_prefixes() {
    let input = "keys sk-abcdef xoxb-12345 xoxp-67890";
    let out = sanitize_api_error(input);
    assert!(!out.contains("sk-abcdef"));
    assert!(!out.contains("xoxb-12345"));
    assert!(!out.contains("xoxp-67890"));
}

#[test]
fn sanitize_short_prefix_then_real_key() {
    let input = "error with sk- prefix and key sk-1234567890";
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-1234567890"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_sk_proj_comment_then_real_key() {
    let input = "note: sk- then sk-proj-abc123def456";
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-proj-abc123def456"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_keeps_bare_prefix() {
    let input = "only prefix sk- present";
    let result = sanitize_api_error(input);
    assert!(result.contains("sk-"));
}

#[test]
fn sanitize_handles_json_wrapped_key() {
    let input = r#"{"error":"invalid key sk-abc123xyz"}"#;
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-abc123xyz"));
}

#[test]
fn sanitize_handles_delimiter_boundaries() {
    let input = "bad token xoxb-abc123}; next";
    let result = sanitize_api_error(input);
    assert!(!result.contains("xoxb-abc123"));
    assert!(result.contains("};"));
}

#[test]
fn sanitize_truncates_long_error() {
    let long = "a".repeat(600);
    let result = sanitize_api_error(&long);
    assert!(result.len() <= 503);
    assert!(result.ends_with("..."));
}

#[test]
fn sanitize_truncates_after_scrub() {
    let input = format!("{} sk-abcdef123456 {}", "a".repeat(290), "b".repeat(290));
    let result = sanitize_api_error(&input);
    assert!(!result.contains("sk-abcdef123456"));
    assert!(result.len() <= 503);
}

#[test]
fn sanitize_preserves_unicode_boundaries() {
    let input = format!("{} sk-abcdef123", "hello🙂".repeat(80));
    let result = sanitize_api_error(&input);
    assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    assert!(!result.contains("sk-abcdef123"));
}

#[test]
fn sanitize_scrubs_gemini_key_in_reqwest_url() {
    let key = "AIzaSyDUMMYKEYFORTESTINGONLY1234567890";
    let input = format!(
        "error sending request for url (https://127.0.0.1:1/v1beta/models/x:generateContent?key={key})"
    );
    let result = sanitize_api_error(&input);
    assert!(!result.contains(key), "key leaked: {result}");
    assert!(result.contains("generateContent"));
    assert!(!result.contains("?key="));
}

#[test]
fn sanitize_scrubs_bare_gemini_key_prefix() {
    // An `AIza` key that appears outside a query string (e.g. a JSON body)
    // is still redacted by the prefix rule.
    let input = r#"{"error":"invalid api key AIzaSyABCDEF0123456789abcdefGHIJKLmnopqrs"}"#;
    let result = sanitize_api_error(input);
    assert!(!result.contains("AIzaSyABCDEF0123456789abcdefGHIJKLmnopqrs"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_removes_complete_url_query_regardless_of_parameter_name() {
    let input = "GET HTTPS://api.example.com/v1/thing?region=us&access_token=hunter2secret failed";
    let result = sanitize_api_error(input);
    assert!(!result.contains("hunter2secret"), "{result}");
    assert!(!result.contains("region=us"), "{result}");
    assert!(result.contains("HTTPS://api.example.com/v1/thing"));
}

#[test]
fn sanitize_removes_query_values_containing_url_punctuation() {
    let secret = "abc,def'ghi(jkl)";
    let input = format!("GET https://api.example.com/v1/thing?api_key={secret} failed");
    let result = sanitize_api_error(&input);

    assert!(!result.contains(secret), "{result}");
    assert!(!result.contains("def'ghi(jkl)"), "{result}");
    assert!(result.contains("https://api.example.com/v1/thing"));
}

#[test]
fn sanitize_leaves_non_url_key_value_text_unchanged() {
    let input = "playing monkey=banana in the query";
    let result = sanitize_api_error(input);
    assert_eq!(result, input);
}

#[test]
fn sanitize_no_secret_no_change() {
    let input = "simple upstream timeout";
    let result = sanitize_api_error(input);
    assert_eq!(result, input);
}

#[test]
fn scrub_github_personal_access_token() {
    let input = "auth failed with token ghp_abc123def456";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "auth failed with token [REDACTED]");
}

#[test]
fn scrub_github_oauth_token() {
    let input = "Bearer gho_1234567890abcdef";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "Bearer [REDACTED]");
}

#[test]
fn scrub_github_user_token() {
    let input = "token ghu_sessiontoken123";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "token [REDACTED]");
}

#[test]
fn scrub_github_fine_grained_pat() {
    let input = "failed: github_pat_11AABBC_xyzzy789";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "failed: [REDACTED]");
}

#[test]
fn provider_runtime_options_default_has_empty_extra_headers() {
    let options = ModelProviderRuntimeOptions::default();
    assert!(options.extra_headers.is_empty());
}

#[test]
fn provider_runtime_options_extra_headers_passed_through() {
    let mut extra_headers = std::collections::HashMap::new();
    extra_headers.insert("X-Title".to_string(), "zeroclaw".to_string());
    let options = ModelProviderRuntimeOptions {
        extra_headers,
        ..ModelProviderRuntimeOptions::default()
    };
    assert_eq!(options.extra_headers.len(), 1);
    assert_eq!(options.extra_headers.get("X-Title").unwrap(), "zeroclaw");
}

/// Build a `Config` with two `anthropic` aliases at different base_urls
/// so the test can prove `provider_runtime_options_for_agent` selects
/// the alias-specific entry via explicit `<type>.<alias>` resolution.
fn config_with_two_openai_aliases() -> reiclaw_config::schema::Config {
    use reiclaw_config::schema::{
        AliasedAgentConfig, Config, ModelProviderConfig, OpenAIModelProviderConfig,
    };
    let mut config = Config::default();
    let default_alias = OpenAIModelProviderConfig {
        base: ModelProviderConfig {
            model: Some("claude-default".into()),
            api_key: Some("default-key".into()),
            uri: Some("https://api.default.example/v1/messages".into()),
            ..ModelProviderConfig::default()
        },
    };
    let work_alias = OpenAIModelProviderConfig {
        base: ModelProviderConfig {
            model: Some("claude-work".into()),
            api_key: Some("work-key".into()),
            uri: Some("https://work-proxy.example/v1/v1/anthropic/messages".into()),
            ..ModelProviderConfig::default()
        },
    };
    config
        .providers
        .models
        .openai
        .insert("default".to_string(), default_alias);
    config
        .providers
        .models
        .openai
        .insert("work".to_string(), work_alias);
    let work_agent = AliasedAgentConfig {
        model_provider: "openai.work".into(),
        ..AliasedAgentConfig::default()
    };
    config.agents.insert("work_agent".to_string(), work_agent);
    let default_agent = AliasedAgentConfig {
        model_provider: "openai.default".into(),
        ..AliasedAgentConfig::default()
    };
    config
        .agents
        .insert("default_agent".to_string(), default_agent);
    config
}

#[test]
fn provider_runtime_options_for_agent_resolves_alias_specific_uri() {
    let config = config_with_two_openai_aliases();
    let work = provider_runtime_options_for_agent(&config, "work_agent");
    let dflt = provider_runtime_options_for_agent(&config, "default_agent");

    assert_eq!(
        work.provider_api_url.as_deref(),
        Some("https://work-proxy.example/v1/v1/anthropic/messages"),
        "work agent must resolve to the work alias's full uri (with merged path)"
    );
    assert_eq!(
        dflt.provider_api_url.as_deref(),
        Some("https://api.default.example/v1/messages"),
        "default agent must resolve to the default alias's full uri (with merged path)"
    );
}

#[test]
fn provider_runtime_options_for_agent_unknown_agent_returns_safe_defaults() {
    let config = config_with_two_openai_aliases();
    let opts = provider_runtime_options_for_agent(&config, "nonexistent");
    assert!(
        opts.provider_api_url.is_none(),
        "unknown agent must not silently inherit any configured provider; got `{:?}`",
        opts.provider_api_url
    );
}

fn config_with_openai_alias() -> reiclaw_config::schema::Config {
    use reiclaw_config::schema::{
        AliasedAgentConfig, Config, ModelProviderConfig, OpenAIModelProviderConfig,
    };
    let mut config = Config::default();
    let alias = OpenAIModelProviderConfig {
        base: ModelProviderConfig {
            api_key: Some("openai-alias-key".into()),
            model: Some("gpt-4o".into()),
            ..ModelProviderConfig::default()
        },
    };
    config
        .providers
        .models
        .openai
        .insert("alias".to_string(), alias);
    let agent = AliasedAgentConfig {
        model_provider: "openai.alias".into(),
        ..AliasedAgentConfig::default()
    };
    config.agents.insert("test_agent".to_string(), agent);
    config
}

#[test]
fn routed_model_provider_credential_precedence_uses_route_key_first() {
    let config = config_with_openai_alias();
    let reliability = reiclaw_config::schema::ReliabilityConfig::default();
    let routes = [reiclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai.alias".into(),
        model: "gpt-4o".into(),
        api_key: Some("route-key".into()),
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai.alias",
        Some("fallback-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "route-key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_credential_precedence_uses_config_entry_key() {
    let config = config_with_openai_alias();
    let reliability = reiclaw_config::schema::ReliabilityConfig::default();
    // Route has no api_key — should fall back to config entry key "openai-alias-key"
    let routes = [reiclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai.alias".into(),
        model: "gpt-4o".into(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai.alias",
        Some("fallback-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "config-entry key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_credential_precedence_falls_back_to_api_key_param() {
    let config = reiclaw_config::schema::Config::default(); // no entry in config.models
    let reliability = reiclaw_config::schema::ReliabilityConfig::default();
    // Neither route nor config entry has api_key — should use the param "fallback-key"
    let routes = [reiclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai".into(),
        model: "gpt-4o".into(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai",
        Some("fallback-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "fallback-key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_credential_skips_config_entry_for_non_dotted_name() {
    let config = reiclaw_config::schema::Config::default();
    let reliability = reiclaw_config::schema::ReliabilityConfig::default();
    // Non-dotted name "openai" — split_once('.') returns None, so config entry
    // lookup is skipped entirely. Falls back to api_key param.
    let routes = [reiclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai".into(),
        model: "gpt-4o".into(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai",
        Some("direct-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "direct-key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_fails_when_routed_provider_fallback_lacks_profile_credential() {
    use reiclaw_config::schema::{
        Config, ModelProviderConfig, ModelRouteConfig, OpenAIModelProviderConfig, ReliabilityConfig,
    };

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "routed".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                api_key: Some("route-key".to_string()),
                fallback: vec![reiclaw_config::providers::ModelProviderRef::new(
                    "openai.bad",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "bad".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1-mini".to_string()),
                ..Default::default()
            },
        },
    );
    let routes = [ModelRouteConfig {
        hint: "test".to_string(),
        model_provider: "openai.routed".to_string(),
        model: "gpt-4.1".to_string(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai.primary",
        Some("primary-key"),
        None,
        &ReliabilityConfig::default(),
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_err(),
        "route provider fallback failures must not be silently ignored"
    );
    let message = result.err().unwrap().to_string();
    assert!(
        message.contains("openai.routed"),
        "error must name the routed provider that failed: {message}"
    );
    assert!(
        message.contains("openai.bad"),
        "error must preserve the failed fallback alias: {message}"
    );
}

#[test]
fn resilient_alias_fails_when_resolved_fallback_lacks_profile_credential() {
    use reiclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![reiclaw_config::providers::ModelProviderRef::new(
                    "openai.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "backup".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &reiclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_err(),
        "missing fallback credential must fail loudly"
    );
    let err = result.err().unwrap();
    let message = err.to_string();
    assert!(
        message.contains("openai.backup"),
        "error must name resolved fallback alias: {message}"
    );
    assert!(
        message.contains("[providers.models.openai.backup]"),
        "error must name the profile to fix: {message}"
    );
    assert!(
        message.contains("api_key"),
        "error must name the credential field to set: {message}"
    );
}

#[test]
fn resilient_alias_fails_when_resolved_fallback_uri_lacks_profile_credential() {
    use reiclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![reiclaw_config::providers::ModelProviderRef::new(
                    "openai.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "backup".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                kind: Some("openai".to_string()),
                uri: Some("https://api.openai.com/v1".to_string()),
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &reiclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_err(),
        "uri and kind alone must not make a fallback auth-ready"
    );
    let message = result.err().unwrap().to_string();
    assert!(
        message.contains("openai.backup"),
        "error must name resolved fallback alias: {message}"
    );
    assert!(
        message.contains("[providers.models.openai.backup]"),
        "error must name the profile to fix: {message}"
    );
    assert!(
        message.contains("api_key"),
        "error must name the credential field to set: {message}"
    );
}

#[test]
fn resilient_alias_fails_when_resolved_fallback_factory_fails() {
    use reiclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        reiclaw_config::schema::OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![reiclaw_config::providers::ModelProviderRef::new(
                    "openai.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "backup".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("router-model".to_string()),
                api_key: Some("router-key".to_string()),
                uri: Some("invalid-url".to_string()),
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &reiclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_err(),
        "resolved fallback factory failure must abort build"
    );
    let err = result.err().unwrap();
    let message = err.to_string();
    assert!(
        message.contains("openai.backup"),
        "error must name resolved fallback alias: {message}"
    );
    assert!(
        message.contains("[providers.models.openai.backup]"),
        "error must name the profile to fix: {message}"
    );
    assert!(
        message.contains("valid URL"),
        "factory error must preserve the missing field detail: {message}"
    );
}

#[test]
fn resilient_alias_dangling_fallback_does_not_abort_build() {
    use reiclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                fallback: vec![reiclaw_config::providers::ModelProviderRef::new(
                    "openai.ghost",
                )],
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        None,
        None,
        &reiclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "a dangling fallback ref must be skipped, never abort the build"
    );
}

#[test]
fn resilient_alias_cyclic_fallback_does_not_loop_or_abort() {
    use reiclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "a".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("a-key".to_string()),
                fallback: vec![reiclaw_config::providers::ModelProviderRef::new("openai.b")],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "b".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                api_key: Some("b-key".to_string()),
                fallback: vec![reiclaw_config::providers::ModelProviderRef::new("openai.a")],
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "a",
        None,
        None,
        &reiclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "a fallback cycle must be pruned, never loop or abort the build"
    );
}

#[test]
fn resilient_alias_deep_acyclic_fallback_does_not_overflow() {
    use reiclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    let n = reiclaw_config::providers::MAX_FALLBACK_DEPTH + 50;
    for i in 0..n {
        let fallback = if i + 1 < n {
            vec![reiclaw_config::providers::ModelProviderRef::new(format!(
                "openai.a{}",
                i + 1
            ))]
        } else {
            vec![]
        };
        config.providers.models.openai.insert(
            format!("a{i}"),
            OpenAIModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("gpt-4o".to_string()),
                    api_key: Some(format!("a{i}-key")),
                    fallback,
                    ..Default::default()
                },
            },
        );
    }

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "a0",
        None,
        None,
        &reiclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "a deep acyclic chain must be depth-capped, never overflow or abort the build"
    );
}

#[test]
fn retired_model_providers_fail_before_alias_or_kind_override_can_reroute() {
    let config = reiclaw_config::schema::Config::default();
    for retired in reiclaw_config::providers::retired_model_provider_names() {
        for name in [retired.to_string(), format!("{retired}.private-alias")] {
            let options = ModelProviderRuntimeOptions {
                provider_kind: Some("openai".to_string()),
                ..ModelProviderRuntimeOptions::default()
            };
            let err = create_model_provider_with_options(&name, Some("private-test-key"), &options)
                .err()
                .expect("retired provider must not route through a retained provider")
                .to_string();
            assert!(err.contains("is retired"), "{retired}: {err}");
            assert!(!err.contains("private-test-key"));
            assert!(!err.contains("private-alias"));
        }
        let options = ModelProviderRuntimeOptions {
            provider_kind: Some(retired.to_string()),
            ..ModelProviderRuntimeOptions::default()
        };
        assert!(
            create_model_provider_for_alias_with_url(
                &config,
                "openai",
                "test",
                Some("private-test-key"),
                None,
                &options,
            )
            .err()
            .expect("retired kind override must fail")
            .to_string()
            .contains("is retired")
        );
        assert!(!canonical_model_provider_slots().contains(&retired));
    }
    assert!(create_model_provider("openai", Some("test-key")).is_ok());
}
