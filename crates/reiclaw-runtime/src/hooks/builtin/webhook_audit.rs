use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::hooks::current_hook_call_id;
use crate::hooks::traits::{HookHandler, HookResult};
use crate::security::estop::EstopHandle;
use reiclaw_api::tool::ToolResult;
use reiclaw_config::schema::WebhookAuditConfig;
use reiclaw_config::{
    validate_webhook_audit_url as validate_webhook_url, validate_webhook_resolved_ip,
};

const MAX_PENDING_CALL_IDS: usize = 1024;

#[derive(Default)]
struct PendingArgsStore {
    by_call_id: HashMap<String, Value>,
    by_call_order: VecDeque<String>,
    fifo_fallback: HashMap<String, Vec<Value>>,
}

impl PendingArgsStore {
    fn record(&mut self, call_id: Option<String>, tool: &str, args: Value) {
        if let Some(id) = call_id {
            if !self.by_call_id.contains_key(&id) {
                if self.by_call_order.len() >= MAX_PENDING_CALL_IDS
                    && let Some(evicted) = self.by_call_order.pop_front()
                {
                    self.by_call_id.remove(&evicted);
                }
                self.by_call_order.push_back(id.clone());
            }
            self.by_call_id.insert(id, args);
        } else {
            self.fifo_fallback
                .entry(tool.to_string())
                .or_default()
                .push(args);
        }
    }

    fn take(&mut self, call_id: Option<&str>, tool: &str) -> Option<Value> {
        if let Some(id) = call_id {
            return self.by_call_id.remove(id);
        }
        let entry = self.fifo_fallback.get_mut(tool).and_then(|v| {
            if v.is_empty() {
                None
            } else {
                Some(v.remove(0))
            }
        });
        if self.fifo_fallback.get(tool).is_some_and(|v| v.is_empty()) {
            self.fifo_fallback.remove(tool);
        }
        entry
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.by_call_id.is_empty() && self.fifo_fallback.is_empty()
    }
}

/// Sends an HTTP POST with a JSON audit payload for matching tool calls.
pub struct WebhookAuditHook {
    config: WebhookAuditConfig,
    client: reqwest::Client,
    pending_args: Arc<Mutex<PendingArgsStore>>,
    estop: Option<Arc<EstopHandle>>,
    #[cfg(test)]
    allow_loopback_for_test: bool,
}

impl WebhookAuditHook {
    pub fn try_new(config: WebhookAuditConfig) -> anyhow::Result<Self> {
        // Warn if enabled but no URL configured.
        if config.enabled && config.url.is_empty() {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"hook": "webhook-audit"})),
                "webhook-audit hook is enabled but no URL is configured — audit events will be dropped"
            );
        }

        // Validate URL against SSRF if one is provided.
        if !config.url.is_empty()
            && let Err(e) = validate_webhook_url(&config.url)
        {
            ::reiclaw_log::record!(
                ERROR,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(
                        ::serde_json::json!({"hook": "webhook-audit", "error": format!("{}", e)})
                    ),
                "webhook URL validation failed"
            );
            anyhow::bail!("webhook-audit: {e}");
        }

        let client = match reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                ::reiclaw_log::record!(
                    ERROR,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(
                            ::serde_json::json!({"hook": "webhook-audit", "error": format!("{e}")})
                        ),
                    "failed to build webhook HTTP client"
                );
                anyhow::bail!("failed to build webhook HTTP client: {e}");
            }
        };
        Ok(Self {
            config,
            client,
            pending_args: Arc::new(Mutex::new(PendingArgsStore::default())),
            estop: None,
            #[cfg(test)]
            allow_loopback_for_test: false,
        })
    }

    pub fn new(config: WebhookAuditConfig) -> Self {
        Self::try_new(config).unwrap_or_else(|e| panic!("{e}"))
    }

    #[must_use]
    pub fn with_estop(mut self, estop: Option<Arc<EstopHandle>>) -> Self {
        self.estop = estop;
        self
    }
}

/// Simple glob matching: `*` matches any sequence of characters.
fn glob_matches(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }

    let parts: Vec<&str> = pattern.split('*').collect();

    // Edge case: pattern is just "*" (already handled above) or multiple stars
    let mut pos = 0usize;

    // The first segment must match the beginning of the text (unless pattern starts with *)
    if !pattern.starts_with('*') {
        let first = parts[0];
        if !text.starts_with(first) {
            return false;
        }
        pos = first.len();
    }

    // The last segment must match the end of the text (unless pattern ends with *)
    if !pattern.ends_with('*') {
        let last = parts[parts.len() - 1];
        if !text.ends_with(last) {
            return false;
        }
        // Ensure no overlap with the prefix we already consumed
        if text.len() < pos + last.len() {
            // Check for overlap case: e.g. pattern "ab*b" text "ab"
            // pos would be 2 (after "ab"), last is "b", text.len()=2, 2 < 2+1=3 -> false
            return false;
        }
    }

    // Now check that the middle segments appear in order between pos and
    // the end boundary.
    let end_boundary = if pattern.ends_with('*') {
        text.len()
    } else {
        text.len() - parts[parts.len() - 1].len()
    };

    let start_idx = if pattern.starts_with('*') { 0 } else { 1 };
    let end_idx = if pattern.ends_with('*') {
        parts.len()
    } else {
        parts.len() - 1
    };

    for part in &parts[start_idx..end_idx] {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = text[pos..end_boundary].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }

    true
}

/// Returns true if `tool` matches any of the given glob patterns.
fn matches_any_pattern(patterns: &[String], tool: &str) -> bool {
    patterns.iter().any(|p| glob_matches(p, tool))
}

/// Truncate serialised args to `max_bytes`. If 0, no truncation.
/// Uses byte-oriented slicing with char-boundary alignment to avoid
/// mixing byte length comparisons with char-count truncation.
#[allow(clippy::cast_possible_truncation)]
fn truncate_args(args: Value, max_bytes: u64) -> Value {
    if max_bytes == 0 {
        return args;
    }
    let serialised = match serde_json::to_string(&args) {
        Ok(s) => s,
        Err(_) => return args,
    };
    if serialised.len() <= max_bytes as usize {
        args
    } else {
        let mut end = max_bytes as usize;
        while end > 0 && !serialised.is_char_boundary(end) {
            end -= 1;
        }
        Value::String(format!("{}...[truncated]", &serialised[..end]))
    }
}

#[async_trait]
impl HookHandler for WebhookAuditHook {
    fn name(&self) -> &str {
        "webhook-audit"
    }

    fn priority(&self) -> i32 {
        -100
    }

    async fn before_tool_call(&self, name: String, args: Value) -> HookResult<(String, Value)> {
        if self.config.include_args && matches_any_pattern(&self.config.tool_patterns, &name) {
            ::reiclaw_log::record!(
                DEBUG,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"hook": "webhook-audit", "tool": name})),
                "capturing args for audit"
            );
            self.pending_args
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(current_hook_call_id(), &name, args.clone());
        }
        HookResult::Continue((name, args))
    }

    async fn on_after_tool_call(&self, tool: &str, result: &ToolResult, duration: Duration) {
        // Skip if no URL configured.
        if self.config.url.is_empty() {
            return;
        }

        // Skip tools that don't match the configured patterns.
        if !matches_any_pattern(&self.config.tool_patterns, tool) {
            return;
        }

        let call_id = current_hook_call_id();
        // Pop captured args for this call (correlated by call_id when scoped).
        let args_value: Value = if self.config.include_args {
            let raw = self
                .pending_args
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(call_id.as_deref(), tool);
            match raw {
                Some(a) => truncate_args(a, self.config.max_args_bytes),
                None => Value::Null,
            }
        } else {
            Value::Null
        };

        if let Some(ref estop) = self.estop {
            let url_host = reqwest::Url::parse(&self.config.url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_string));
            if let Err(reason) = estop.enforce_network(url_host.as_deref()) {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"hook": "webhook-audit", "tool": tool, "reason": reason})),
                    "webhook-audit suppressed by emergency stop"
                );
                return;
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = duration.as_millis() as u64;

        let payload = serde_json::json!({
            "event": "tool_call",
            "timestamp": reiclaw_api::time::format_rfc3339(jiff::Timestamp::now()),
            "tool": tool,
            "success": result.success,
            "duration_ms": duration_ms,
            "error": result.error,
            "args": args_value,
        });

        let url = self.config.url.clone();
        let _base_client = &self.client;
        #[cfg(test)]
        let allow_loopback = self.allow_loopback_for_test;
        #[cfg(not(test))]
        let allow_loopback = false;

        // Fire-and-forget — never block the agent loop.
        reiclaw_log::spawn!(async move {
            let client = match resolve_and_pin_webhook_client(&url, allow_loopback).await {
                Ok(client) => client,
                Err(e) => {
                    ::reiclaw_log::record!(
                        ERROR,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                            .with_outcome(::reiclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"hook": "webhook-audit", "url": url, "error": format!("{e}")})),
                        "webhook-audit blocked by SSRF/DNS validation"
                    );
                    return;
                }
            };
            match client.post(&url).json(&payload).send().await {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        ::reiclaw_log::record!(ERROR, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail).with_outcome(::reiclaw_log::EventOutcome::Failure).with_attrs(::serde_json::json!({"hook": "webhook-audit", "url": url, "status": resp.status().to_string()})), "webhook endpoint returned non-success status");
                    }
                }
                Err(e) => {
                    ::reiclaw_log::record!(WARN, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_outcome(::reiclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"hook": "webhook-audit", "url": url, "error": format!("{}", e)})), "failed to POST audit payload");
                }
            }
        });
    }
}

async fn resolve_and_pin_webhook_client(
    url: &str,
    allow_loopback_for_test: bool,
) -> anyhow::Result<reqwest::Client> {
    let parsed = reqwest::Url::parse(url).map_err(|e| anyhow::Error::msg(format!("{e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::Error::msg("webhook URL missing host"))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let bare = host.trim_start_matches('[').trim_end_matches(']');

    let validate_ip = |ip: std::net::IpAddr| -> anyhow::Result<()> {
        if allow_loopback_for_test && ip.is_loopback() {
            return Ok(());
        }
        validate_webhook_resolved_ip(ip).map_err(anyhow::Error::msg)
    };

    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5));

    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        validate_ip(ip)?;
    } else {
        let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host((bare, port))
            .await
            .map_err(|e| anyhow::Error::msg(format!("DNS lookup failed for {bare}: {e}")))?
            .collect();
        if resolved.is_empty() {
            anyhow::bail!("DNS lookup returned no addresses for {bare}");
        }
        for addr in &resolved {
            validate_ip(addr.ip())?;
        }
        builder = builder.resolve_to_addrs(bare, &resolved);
    }

    builder
        .build()
        .map_err(|e| anyhow::Error::msg(format!("failed to build pinned webhook client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Glob matching tests ──────────────────────────────────────

    #[test]
    fn glob_exact_match() {
        assert!(glob_matches("file_write", "file_write"));
        assert!(!glob_matches("file_write", "file_read"));
    }

    #[test]
    fn glob_wildcard_suffix() {
        assert!(glob_matches("mcp__*", "mcp__github"));
        assert!(glob_matches("mcp__*", "mcp__"));
        assert!(!glob_matches("mcp__*", "mcp_github"));
    }

    #[test]
    fn glob_wildcard_prefix() {
        assert!(glob_matches("*_write", "file_write"));
        assert!(glob_matches("*_write", "_write"));
        assert!(!glob_matches("*_write", "file_read"));
    }

    #[test]
    fn glob_wildcard_middle() {
        assert!(glob_matches("mcp__*__create", "mcp__github__create"));
        assert!(glob_matches("mcp__*__create", "mcp____create"));
        assert!(!glob_matches("mcp__*__create", "mcp__github__delete"));
    }

    #[test]
    fn glob_star_matches_everything() {
        assert!(glob_matches("*", "anything_at_all"));
        assert!(glob_matches("*", ""));
    }

    #[test]
    fn glob_empty_pattern() {
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "something"));
    }

    // ── matches_any_pattern ──────────────────────────────────────

    #[test]
    fn matches_any_pattern_works() {
        let patterns = vec!["Bash".to_string(), "mcp__*".to_string()];
        assert!(matches_any_pattern(&patterns, "Bash"));
        assert!(matches_any_pattern(&patterns, "mcp__github"));
        assert!(!matches_any_pattern(&patterns, "Write"));
    }

    #[test]
    fn empty_patterns_matches_nothing() {
        let patterns: Vec<String> = vec![];
        assert!(!matches_any_pattern(&patterns, "anything"));
    }

    // ── before_tool_call tests ────────────────────────────────────

    fn make_hook(patterns: Vec<&str>, include_args: bool) -> WebhookAuditHook {
        // Use https URL for tests to pass URL validation; localhost with http
        // is only allowed in debug builds, but use https to be safe.
        WebhookAuditHook::new(WebhookAuditConfig {
            enabled: true,
            url: "https://audit.example.com/webhook".to_string(),
            tool_patterns: patterns.into_iter().map(String::from).collect(),
            include_args,
            max_args_bytes: 4096,
        })
    }

    #[tokio::test]
    async fn before_tool_call_captures_args_when_enabled() {
        let hook = make_hook(vec!["Bash", "mcp__*"], true);
        let args = serde_json::json!({"command": "ls"});
        let result = hook.before_tool_call("Bash".into(), args.clone()).await;
        assert!(!result.is_cancel());

        let pending = hook.pending_args.lock().unwrap();
        assert_eq!(pending.fifo_fallback.get("Bash"), Some(&vec![args]));
    }

    #[tokio::test]
    async fn before_tool_call_concurrent_same_tool_no_data_loss() {
        let hook = make_hook(vec!["Bash"], true);
        let args1 = serde_json::json!({"command": "ls"});
        let args2 = serde_json::json!({"command": "pwd"});
        hook.before_tool_call("Bash".into(), args1.clone()).await;
        hook.before_tool_call("Bash".into(), args2.clone()).await;

        let pending = hook.pending_args.lock().unwrap();
        let bash_args = pending.fifo_fallback.get("Bash").unwrap();
        assert_eq!(bash_args.len(), 2);
        assert_eq!(bash_args[0], args1);
        assert_eq!(bash_args[1], args2);
    }

    #[tokio::test]
    async fn before_tool_call_skips_non_matching_tools() {
        let hook = make_hook(vec!["Bash"], true);
        let args = serde_json::json!({"path": "/tmp"});
        let result = hook.before_tool_call("Write".into(), args).await;
        assert!(!result.is_cancel());

        let pending = hook.pending_args.lock().unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn before_tool_call_skips_when_include_args_false() {
        let hook = make_hook(vec!["Bash"], false);
        let args = serde_json::json!({"command": "ls"});
        let result = hook.before_tool_call("Bash".into(), args).await;
        assert!(!result.is_cancel());

        let pending = hook.pending_args.lock().unwrap();
        assert!(pending.is_empty());
    }

    // ── Truncation tests ─────────────────────────────────────────

    #[test]
    fn truncate_args_within_limit() {
        let args = serde_json::json!({"key": "val"});
        let result = truncate_args(args.clone(), 1000);
        assert_eq!(result, args);
    }

    #[test]
    fn truncate_args_over_limit() {
        let args = serde_json::json!({"key": "a]long value that exceeds limit"});
        let result = truncate_args(args, 10);
        assert!(result.is_string());
        let s = result.as_str().unwrap();
        assert!(s.ends_with("...[truncated]"));
    }

    #[test]
    fn truncate_args_zero_means_no_limit() {
        let args = serde_json::json!({"key": "value"});
        let result = truncate_args(args.clone(), 0);
        assert_eq!(result, args);
    }

    // ── on_after_tool_call tests ─────────────────────────────────

    #[tokio::test]
    async fn on_after_tool_call_skips_non_matching() {
        let hook = make_hook(vec!["Bash"], true);
        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        // Call with a non-matching tool — should not panic or do anything.
        hook.on_after_tool_call("Write", &result, Duration::from_millis(10))
            .await;
        // No assertion needed beyond "doesn't panic"; args map stays empty.
        let pending = hook.pending_args.lock().unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn on_after_tool_call_skips_empty_url() {
        // Empty URL + enabled triggers a warning, but should not panic.
        let hook = WebhookAuditHook::new(WebhookAuditConfig {
            enabled: true,
            url: String::new(),
            tool_patterns: vec!["Bash".to_string()],
            include_args: false,
            max_args_bytes: 4096,
        });
        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        // Should return immediately without spawning any HTTP request.
        hook.on_after_tool_call("Bash", &result, Duration::from_millis(5))
            .await;
    }

    // ── URL validation tests ─────────────────────────────────────

    #[test]
    fn validate_url_rejects_loopback_ipv4() {
        assert!(validate_webhook_url("https://127.0.0.1/hook").is_err());
        assert!(validate_webhook_url("https://127.0.0.100/hook").is_err());
    }

    #[test]
    fn validate_url_rejects_loopback_ipv6() {
        assert!(validate_webhook_url("https://[::1]/hook").is_err());
    }

    #[test]
    fn validate_url_rejects_private_rfc1918() {
        assert!(validate_webhook_url("https://10.0.0.1/hook").is_err());
        assert!(validate_webhook_url("https://172.16.5.1/hook").is_err());
        assert!(validate_webhook_url("https://192.168.1.1/hook").is_err());
    }

    #[test]
    fn validate_url_rejects_link_local() {
        assert!(validate_webhook_url("https://169.254.1.1/hook").is_err());
        assert!(validate_webhook_url("https://[fe80::1]/hook").is_err());
    }

    #[test]
    fn validate_url_rejects_http_non_localhost() {
        assert!(validate_webhook_url("http://example.com/hook").is_err());
    }

    #[test]
    fn validate_url_accepts_https_public() {
        assert!(validate_webhook_url("https://audit.example.com/webhook").is_ok());
        assert!(validate_webhook_url("https://8.8.8.8/hook").is_ok());
    }

    #[test]
    fn validate_url_rejects_non_http_scheme() {
        assert!(validate_webhook_url("ftp://example.com/hook").is_err());
    }

    #[tokio::test]
    async fn pending_args_call_id_isolation_prevents_cross_contamination() {
        let hook = WebhookAuditHook::try_new(WebhookAuditConfig {
            enabled: true,
            url: String::new(),
            tool_patterns: vec!["shell".to_string()],
            include_args: true,
            max_args_bytes: 4096,
        })
        .unwrap();

        // Call A captures args, then gets cancelled before on_after_tool_call.
        crate::hooks::CURRENT_HOOK_CALL_ID
            .scope(
                "call-a".to_string(),
                hook.before_tool_call("shell".to_string(), serde_json::json!({"cmd": "a"})),
            )
            .await;

        // Call B captures args and completes: it must retrieve only Call B's args.
        crate::hooks::CURRENT_HOOK_CALL_ID
            .scope(
                "call-b".to_string(),
                hook.before_tool_call("shell".to_string(), serde_json::json!({"cmd": "b"})),
            )
            .await;

        let taken_b = hook
            .pending_args
            .lock()
            .unwrap()
            .take(Some("call-b"), "shell");
        assert_eq!(taken_b, Some(serde_json::json!({"cmd": "b"})));
        // Call A's entry did not leak into Call B and remains keyed only under call-a.
        assert_eq!(
            hook.pending_args
                .lock()
                .unwrap()
                .take(Some("call-a"), "shell"),
            Some(serde_json::json!({"cmd": "a"}))
        );
    }

    #[tokio::test]
    async fn webhook_audit_suppressed_by_estop() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("estop.json");
        let engaged = crate::security::estop::EstopState {
            kill_all: false,
            network_kill: true,
            blocked_domains: Vec::new(),
            frozen_tools: Vec::new(),
            updated_at: None,
        };
        std::fs::write(&state_path, serde_json::to_string(&engaged).unwrap()).unwrap();
        let estop = crate::security::estop::EstopHandle::for_path(state_path);

        let hook = WebhookAuditHook {
            config: WebhookAuditConfig {
                enabled: true,
                url: format!("{}/audit", server.uri()),
                tool_patterns: vec!["*".to_string()],
                include_args: false,
                max_args_bytes: 1024,
            },
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(1))
                .build()
                .unwrap(),
            pending_args: Arc::new(Mutex::new(PendingArgsStore::default())),
            estop: Some(estop),
            allow_loopback_for_test: true,
        };

        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        hook.on_after_tool_call("shell", &result, Duration::from_millis(5))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn webhook_audit_rejects_private_dns_resolution() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let port = server.address().port();
        let err = resolve_and_pin_webhook_client(&format!("https://localhost:{port}/audit"), false)
            .await
            .expect_err("localhost DNS resolution must be rejected for webhook delivery");
        assert!(err.to_string().contains("loopback") || err.to_string().contains("private"));
    }

    #[tokio::test]
    async fn webhook_audit_blocks_redirects() {
        let target = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&target)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&target)
            .await;

        let redirector = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/secret", target.uri())),
            )
            .expect(1)
            .mount(&redirector)
            .await;

        let hook = WebhookAuditHook {
            config: WebhookAuditConfig {
                enabled: true,
                url: format!("{}/audit", redirector.uri()),
                tool_patterns: vec!["*".to_string()],
                include_args: false,
                max_args_bytes: 1024,
            },
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(1))
                .build()
                .unwrap(),
            pending_args: Arc::new(Mutex::new(PendingArgsStore::default())),
            estop: None,
            allow_loopback_for_test: true,
        };
        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        hook.on_after_tool_call("shell", &result, Duration::from_millis(5))
            .await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    #[tokio::test]
    async fn webhook_audit_bypasses_proxy_env_and_pins_resolved_addr() {
        static PROXY_ENV_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
            std::sync::OnceLock::new();
        struct ProxyEnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for ProxyEnvGuard {
            fn drop(&mut self) {
                for (key, prev) in &self.0 {
                    // SAFETY: serialized under PROXY_ENV_LOCK in this test.
                    unsafe {
                        match prev {
                            Some(val) => std::env::set_var(key, val),
                            None => std::env::remove_var(key),
                        }
                    }
                }
            }
        }

        let _lock = PROXY_ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;

        let fake_proxy = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(502))
            .expect(0)
            .mount(&fake_proxy)
            .await;

        let target = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&target)
            .await;

        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        let mut saved = Vec::with_capacity(keys.len());
        for key in keys {
            saved.push((key, std::env::var_os(key)));
            // SAFETY: serialized under PROXY_ENV_LOCK in this test.
            unsafe { std::env::set_var(key, fake_proxy.uri()) };
        }
        let _env_guard = ProxyEnvGuard(saved);

        let port = target.address().port();
        let url = format!("http://localhost:{port}/audit");
        let client = resolve_and_pin_webhook_client(&url, true)
            .await
            .expect("pinned webhook client must build");
        let resp = client
            .post(&url)
            .json(&serde_json::json!({"ok": true}))
            .send()
            .await
            .expect("direct pinned webhook delivery must succeed despite proxy env");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
}
