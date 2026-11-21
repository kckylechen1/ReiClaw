//! TG1: ModelProvider End-to-End Resolution Tests

use reiclaw::providers::create_model_provider_with_url;

/// Helper: assert model_provider creation succeeds
fn assert_provider_ok(name: &str, key: Option<&str>, url: Option<&str>) {
    let result = create_model_provider_with_url(name, key, url);
    assert!(
        result.is_ok(),
        "{name} model_provider should resolve: {}",
        result.err().map(|e| e.to_string()).unwrap_or_default()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// OpenAI Responses endpoint resolution and fail-closed rejection
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_openai_with_custom_api_url() {
    assert_provider_ok(
        "openai",
        Some("test-key"),
        Some("https://custom-openai-proxy.example.com/v1"),
    );
}

#[test]
fn factory_openai_ftp_api_url_rejected() {
    let result =
        create_model_provider_with_url("openai", Some("test-key"), Some("ftp://example.com"));
    assert!(result.is_err(), "ftp scheme should be rejected");
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("http") && err_msg.contains("https"),
        "error should mention valid schemes: {err_msg}"
    );
}

#[test]
fn factory_openai_chat_completions_url_rejected() {
    let result = create_model_provider_with_url(
        "openai",
        Some("test-key"),
        Some("https://api.example.com/v1/chat/completions"),
    );
    assert!(
        result.is_err(),
        "chat/completions endpoint should be rejected"
    );
}

#[test]
fn factory_retired_custom_and_anthropic_prefixes_rejected() {
    for retired in [
        "custom:https://api.example.com/v1",
        "anthropic-custom:https://api.example.com",
    ] {
        let result = create_model_provider_with_url(retired, Some("test-key"), None);
        assert!(result.is_err(), "{retired} should fail closed");
    }
}
