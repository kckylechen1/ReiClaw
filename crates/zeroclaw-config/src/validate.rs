//! Configuration validation and non-fatal diagnostics.
//!
//! Declaration, serde-default and existing public schema paths stay in `schema`.

use crate::domain_matcher::DomainMatcher;
use crate::schema::{
    Config, EmbeddingRouteConfig, ExternalRegistry, ExternalRegistryKind,
    MAX_MEMORY_RERANK_CANDIDATE_MULTIPLIER, McpConfig, McpTransport, MemoryConfig,
    ModelProviderConfig, NevisConfig, ProxyConfig, ProxyScope, REPLY_MIN_INTERVAL_MAX_SECS,
    REPLY_QUEUE_DEPTH_CEILING, SearchMode, TEMPERATURE_RANGE, TelegramConfig,
    default_fts_early_return_score, default_otp_challenge_max_attempts, default_otp_gated_actions,
    default_rerank_threshold, default_retrieval_stages, normalize_proxy_url_option,
};
use crate::validation_bail;
use anyhow::{Context, Result};

pub(crate) const SUPPORTED_PROXY_SERVICE_KEYS: &[&str] = &[
    "model_provider.anthropic",
    "model_provider.compatible",
    "model_provider.gemini",
    "model_provider.ollama",
    "model_provider.openai",
    "channel.dingtalk",
    "channel.qq",
    "channel.telegram",
    "channel.wechat",
    "tool.http_request",
    "tool.web_search",
    "memory.embeddings",
    "tunnel.custom",
    "transcription.groq",
];

/// Service keys of retired channels and tools. Still accepted by
/// `ProxyConfig::validate` so a
/// leftover `proxy.services` entry does not fail config load, but no longer
/// advertised: nothing consumes them.
const RETIRED_PROXY_SERVICE_KEYS: &[&str] = &[
    "channel.discord",
    "channel.lark",
    "channel.matrix",
    "channel.mattermost",
    "channel.signal",
    "channel.slack",
    "channel.nextcloud_talk",
    "channel.wati",
    "channel.whatsapp",
    "tool.composio",
    "tool.pushover",
];

pub(crate) const SUPPORTED_PROXY_SERVICE_SELECTORS: &[&str] = &[
    "model_provider.*",
    "channel.*",
    "tool.*",
    "memory.*",
    "tunnel.*",
    "transcription.*",
];

pub(crate) const MCP_MAX_TOOL_TIMEOUT_SECS: u64 = 600;

/// Validate that a temperature value is within the allowed range.
pub fn validate_temperature(value: f64) -> std::result::Result<f64, String> {
    if TEMPERATURE_RANGE.contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "temperature {value} is out of range (expected {}..={})",
            TEMPERATURE_RANGE.start(),
            TEMPERATURE_RANGE.end()
        ))
    }
}

fn is_supported_proxy_service_selector(selector: &str) -> bool {
    if SUPPORTED_PROXY_SERVICE_KEYS
        .iter()
        .chain(RETIRED_PROXY_SERVICE_KEYS)
        .any(|known| known.eq_ignore_ascii_case(selector))
    {
        return true;
    }

    SUPPORTED_PROXY_SERVICE_SELECTORS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(selector))
}

pub(crate) fn validate_mcp_config(config: &McpConfig) -> Result<()> {
    let mut seen_names = std::collections::HashSet::new();
    for (i, server) in config.servers.iter().enumerate() {
        let name = server.name.trim();
        if name.is_empty() {
            validation_bail!(
                RequiredFieldEmpty,
                format!("mcp.servers[{i}].name"),
                "mcp.servers[{i}].name must not be empty"
            );
        }
        if !seen_names.insert(name.to_ascii_lowercase()) {
            anyhow::bail!("mcp.servers contains duplicate name: {name}");
        }

        if let Some(timeout) = server.tool_timeout_secs {
            if timeout == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    format!("mcp.servers[{i}].tool_timeout_secs"),
                    "mcp.servers[{i}].tool_timeout_secs must be greater than 0"
                );
            }
            if timeout > MCP_MAX_TOOL_TIMEOUT_SECS {
                anyhow::bail!(
                    "mcp.servers[{i}].tool_timeout_secs exceeds max {MCP_MAX_TOOL_TIMEOUT_SECS}"
                );
            }
        }

        // The transport -> required-leaf relationship is owned by
        // `McpTransport::required_leaf`, which feeds the `x-required-by-transport`
        // schema metadata. The validator matches on the `McpTransport` enum so a
        // new variant is a compile error here rather than a runtime fall-through.
        match server.transport {
            McpTransport::Stdio => {
                if server.command.trim().is_empty() {
                    anyhow::bail!(
                        "mcp.servers[{i}] with transport=stdio requires non-empty command"
                    );
                }
            }
            McpTransport::Http | McpTransport::Sse => {
                let url = server
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        let transport_str = server.transport.wire_name();
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                                ::zeroclaw_log::Action::Reject
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "index": i,
                                "transport": transport_str,
                            })),
                            "mcp.servers entry rejected: transport requires url"
                        );
                        anyhow::Error::msg(format!(
                            "mcp.servers[{i}] with transport={transport_str} requires url"
                        ))
                    })?;
                let parsed = url::Url::parse(url)
                    .with_context(|| format!("mcp.servers[{i}].url is not a valid URL"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    anyhow::bail!("mcp.servers[{i}].url must use http/https");
                }
            }
        }
    }
    Ok(())
}

fn validate_proxy_url(field: &str, url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)
        .with_context(|| format!("Invalid {field} URL: '{url}' is not a valid URL"))?;

    match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" | "socks" => {}
        scheme => {
            anyhow::bail!(
                "Invalid {field} URL scheme '{scheme}'. Allowed: http, https, socks5, socks5h, socks"
            );
        }
    }

    if parsed.host_str().is_none() {
        anyhow::bail!("Invalid {field} URL: host is required");
    }

    Ok(())
}

fn validate_http_base_url(field: &str, url: &str) -> Result<()> {
    if url.trim().is_empty() {
        validation_bail!(
            RequiredFieldEmpty,
            field.to_string(),
            "{field} must not be empty"
        );
    }

    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(err) => {
            validation_bail!(
                InvalidFormat,
                field.to_string(),
                "{field} must be a valid URL: {err}"
            );
        }
    };

    if !matches!(parsed.scheme(), "http" | "https") {
        validation_bail!(
            InvalidFormat,
            field.to_string(),
            "{field} must use http:// or https://"
        );
    }

    if parsed.host_str().is_none() {
        validation_bail!(
            InvalidFormat,
            field.to_string(),
            "{field} must include a host"
        );
    }

    Ok(())
}

/// Shared bot-token rule for channel structs whose `bot_token` is required
/// once the alias is enabled (Telegram). `field_path` must be the
/// `channels.<type>.<alias>.bot_token` leaf; the enabled-state message
/// derives the sibling `.enabled` path from it.
fn validate_required_bot_token(field_path: &str, enabled: bool, token: &str) -> Result<()> {
    if token.trim() == crate::traits::UNSET_DISPLAY {
        validation_bail!(
            RequiredFieldEmpty,
            field_path.to_string(),
            "{field_path} must not contain the unset display placeholder",
        );
    }
    if enabled && crate::traits::is_unset_display_value(token) {
        let enabled_path = field_path.strip_suffix("bot_token").map_or_else(
            || field_path.to_string(),
            |prefix| format!("{prefix}enabled"),
        );
        validation_bail!(
            RequiredFieldEmpty,
            field_path.to_string(),
            "{field_path} is required when {enabled_path} = true",
        );
    }
    Ok(())
}

fn validate_memory_rerank_config(memory: &MemoryConfig) -> Result<()> {
    if !(1..=MAX_MEMORY_RERANK_CANDIDATE_MULTIPLIER).contains(&memory.candidate_multiplier) {
        anyhow::bail!(
            "memory.candidate_multiplier must be in 1..={MAX_MEMORY_RERANK_CANDIDATE_MULTIPLIER} (got {})",
            memory.candidate_multiplier
        );
    }

    for (path, value) in [
        ("memory.min_relevance_score", memory.min_relevance_score),
        ("memory.mmr_lambda", memory.mmr_lambda),
        ("memory.importance_weight", memory.importance_weight),
        ("memory.recency_weight", memory.recency_weight),
    ] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            anyhow::bail!("{path} must be a finite number in 0.0..=1.0 (got {value})");
        }
    }

    Ok(())
}

/// Surface `[memory]` knobs that the schema accepts but that have no runtime
/// consumer yet, so an operator who sets them learns they currently have no
/// effect instead of debugging missing behavior (same class of check as the
/// `wire_api_not_supported_for_family` warning).
///
/// A PR that wires a consumer for one of these knobs must drop its check
/// here in the same change; this list mirrors what is inert on the current
/// tree, not what is planned.
///
/// Called from `Config::collect_warnings`, so each warning reaches both the
/// CLI (via `validate()`'s tracing emission) and the gateway dashboard.
pub fn validate_memory_semantics(
    memory: &MemoryConfig,
) -> Vec<crate::validation_warnings::ValidationWarning> {
    let mut inert: Vec<(&'static str, &'static str)> = Vec::new();

    // The staged retrieval pipeline (`RetrievalPipeline`) exists but is not
    // wired into the production recall path, so its tuning knobs are inert.
    if memory.retrieval_stages != default_retrieval_stages() {
        inert.push((
            "memory.retrieval_stages",
            "the staged retrieval pipeline is not wired into the recall path yet",
        ));
    }
    if (memory.fts_early_return_score - default_fts_early_return_score()).abs() > f64::EPSILON {
        inert.push((
            "memory.fts_early_return_score",
            "the staged retrieval pipeline is not wired into the recall path yet",
        ));
    }

    // The proposed rerank stage was never landed, so these knobs remain inert.
    // `DefaultMemoryStrategy::new` logs the same fact at agent start; this
    // config-time copy reaches `config validate` and dashboard callers too.
    if memory.rerank_enabled {
        inert.push((
            "memory.rerank_enabled",
            "the rerank stage is not yet implemented",
        ));
    }
    if memory.rerank_threshold != default_rerank_threshold() {
        inert.push((
            "memory.rerank_threshold",
            "the rerank stage is not yet implemented",
        ));
    }

    inert
        .into_iter()
        .map(|(path, reason)| {
            crate::validation_warnings::ValidationWarning::new(
                "memory_config_knob_inert",
                format!("{path} is set but {reason}; this setting currently has no effect"),
                path,
            )
        })
        .collect()
}

fn is_valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_local_ollama_endpoint(api_url: Option<&str>) -> bool {
    let Some(raw) = api_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };

    url::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"))
}

fn is_official_ollama_cloud_endpoint(api_url: Option<&str>) -> bool {
    let Some(raw) = api_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };

    url::Url::parse(raw)
        .ok()
        .and_then(|url| {
            url.host_str().map(|host| {
                host.eq_ignore_ascii_case("ollama.com")
                    || host.eq_ignore_ascii_case("api.ollama.com")
            })
        })
        .unwrap_or(false)
}

fn has_ollama_cloud_credential(config_api_key: Option<&str>) -> bool {
    config_api_key
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

impl ProxyConfig {
    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("http_proxy", self.http_proxy.as_deref()),
            ("https_proxy", self.https_proxy.as_deref()),
            ("all_proxy", self.all_proxy.as_deref()),
        ] {
            if let Some(url) = normalize_proxy_url_option(value) {
                validate_proxy_url(field, &url)?;
            }
        }

        for selector in self.normalized_services() {
            if !is_supported_proxy_service_selector(&selector) {
                anyhow::bail!(
                    "Unsupported proxy service selector '{selector}'. Use tool `proxy_config` action `list_services` for valid values"
                );
            }
        }

        if self.enabled && !self.has_any_proxy_url() {
            anyhow::bail!(
                "Proxy is enabled but no proxy URL is configured. Set at least one of http_proxy, https_proxy, or all_proxy"
            );
        }

        if self.enabled
            && self.scope == ProxyScope::Services
            && self.normalized_services().is_empty()
        {
            anyhow::bail!(
                "proxy.scope='services' requires a non-empty proxy.services list when proxy is enabled"
            );
        }

        Ok(())
    }
}

impl TelegramConfig {
    /// Validate this alias's bot-token placeholder and enabled-state rules.
    pub fn validate_bot_token(&self, alias: &str) -> Result<()> {
        validate_required_bot_token(
            &format!("channels.telegram.{alias}.bot_token"),
            self.enabled,
            &self.bot_token,
        )
    }
}

impl NevisConfig {
    /// Validate that required fields are present when Nevis is enabled.
    ///
    /// Call at config load time to fail fast on invalid configuration rather
    /// than deferring errors to the first authentication request.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        if self.instance_url.trim().is_empty() {
            return Err("nevis.instance_url is required when Nevis IAM is enabled".into());
        }

        if self.client_id.trim().is_empty() {
            return Err("nevis.client_id is required when Nevis IAM is enabled".into());
        }

        if self.realm.trim().is_empty() {
            return Err("nevis.realm is required when Nevis IAM is enabled".into());
        }

        match self.token_validation.as_str() {
            "local" | "remote" => {}
            other => {
                return Err(format!(
                    "nevis.token_validation has invalid value '{other}': \
                     expected 'local' or 'remote'"
                ));
            }
        }

        if self.token_validation == "local" && self.jwks_url.is_none() {
            return Err("nevis.jwks_url is required when token_validation is 'local'".into());
        }

        if self.session_timeout_secs == 0 {
            return Err("nevis.session_timeout_secs must be greater than 0".into());
        }

        Ok(())
    }
}

impl Config {
    /// Collect non-fatal validation warnings — config that loads and
    /// validates successfully (`validate()` returns `Ok(())`) but will fail
    /// at runtime because of a logical inconsistency the schema cannot
    /// enforce structurally.
    ///
    /// Called by `validate()` (which emits each warning via `tracing::warn!`
    /// for log visibility) and by the gateway HTTP API (which returns the
    /// structured list in `PropResponse` / `PatchResponse` so dashboard
    /// callers see the same signal the CLI sees on stderr).
    ///
    /// Adding a new warning: append a check here, pick a stable `code`,
    /// and document the code in `validation_warnings.rs`.
    pub fn collect_warnings(&self) -> Vec<crate::validation_warnings::ValidationWarning> {
        let mut warnings = Vec::new();
        // Load-time observations (retired env prefixes, retired file
        // sections) have no other in-config representation; replay them so
        // the structured warning surface stays the single exit point.
        warnings.extend(self.retired_surface_warnings.iter().cloned());
        self.collect_fallback_warnings(&mut warnings);
        self.collect_cross_provider_summary_model_warnings(&mut warnings);
        self.collect_memory_semantic_search_warnings(&mut warnings);
        // Must run after `collect_cross_provider_summary_model_warnings`: it
        // scans `warnings` to suppress its generic inert `summary_model`
        // warning when the more specific cross-provider diagnostic already
        // covers the same path.
        self.collect_context_compression_ignored_warnings(&mut warnings);
        self.collect_deprecated_otp_action_gating_warnings(&mut warnings);
        warnings.extend(validate_memory_semantics(&self.memory));
        // `wire_api` is only honored by bring-your-own-endpoint families; on a
        // branded family with a fixed wire protocol it is silently ignored.
        // Surface that so an operator who sets it on, e.g., `mistral` learns it
        // has no effect instead of debugging unexpected runtime behavior.
        for (family, alias, entry) in self.providers.models.iter_entries() {
            if entry.wire_api.is_some() && !crate::provider_aliases::family_honors_wire_api(family)
            {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "wire_api_not_supported_for_family",
                    format!(
                        "wire_api is set on `{family}.{alias}` but the `{family}` family has a \
                         fixed wire protocol and ignores it. wire_api only takes effect on the \
                         openai, llamacpp, and custom (openai-compatible) families."
                    ),
                    format!("providers.models.{family}.{alias}.wire_api"),
                ));
            }
        }
        warnings
    }

    /// Surface tachi semantic/hybrid search with no effective embedder. The
    /// runtime treats `NoopEmbedding` as dimensions 0 and silently skips the
    /// vector path, so derive this from the canonical memory settings instead
    /// of storing another effective-mode field.
    fn collect_memory_semantic_search_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        if !Self::memory_backend_is_tachi(&self.memory.backend) {
            return;
        }
        if !matches!(
            &self.memory.search_mode,
            SearchMode::Embedding | SearchMode::Hybrid
        ) {
            return;
        }
        if self.memory_has_effective_embedder() {
            return;
        }

        let search_mode = match &self.memory.search_mode {
            SearchMode::Bm25 => "bm25",
            SearchMode::Embedding => "embedding",
            SearchMode::Hybrid => "hybrid",
        };
        warnings.push(crate::validation_warnings::ValidationWarning::new(
            "memory_semantic_search_without_embedder",
            format!(
                "memory.search_mode is {search_mode:?} on tachi memory, but no effective \
                 embedding provider is configured; vector search is skipped and \
                 recall falls back to keyword-only behavior. Configure \
                 memory.embedding_provider or a valid memory.embedding_model \
                 hint route, or set memory.search_mode = \"bm25\"."
            ),
            "memory.search_mode",
        ));
    }

    fn memory_backend_is_tachi(backend: &str) -> bool {
        let trimmed = backend.trim();
        let family = trimmed
            .split_once('.')
            .map_or(trimmed, |(family, _)| family)
            .trim();
        family.eq_ignore_ascii_case("tachi")
    }

    fn memory_has_effective_embedder(&self) -> bool {
        let fallback = || Self::memory_embedding_settings_have_effective_embedder(&self.memory);
        let Some(hint) = self
            .memory
            .embedding_model
            .strip_prefix("hint:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return fallback();
        };

        let Some(route) = self
            .embedding_routes
            .iter()
            .find(|route| route.hint.trim() == hint)
        else {
            return fallback();
        };

        if Self::embedding_route_has_effective_embedder(route, self.memory.embedding_dimensions) {
            return true;
        }
        fallback()
    }

    fn memory_embedding_settings_have_effective_embedder(memory: &MemoryConfig) -> bool {
        let provider = memory.embedding_provider.trim();
        !provider.is_empty()
            && !provider.eq_ignore_ascii_case("none")
            && memory.embedding_dimensions > 0
    }

    fn embedding_route_has_effective_embedder(
        route: &EmbeddingRouteConfig,
        fallback_dimensions: usize,
    ) -> bool {
        let provider = route.model_provider.trim();
        let model = route.model.trim();
        let dimensions = route.dimensions.unwrap_or(fallback_dimensions);
        !provider.is_empty()
            && !provider.eq_ignore_ascii_case("none")
            && !model.is_empty()
            && dimensions > 0
    }

    /// Surface cross-provider ambiguity in a legacy config while reporting
    /// the current contract: context compression has no runtime consumer, so
    /// this knob is inert like every other `context_compression` field (see
    /// `collect_context_compression_ignored_warnings`). This diagnostic adds
    /// config-shape detail as a more specific companion for the same line. The
    /// deprecated
    /// `runtime_profiles.<p>.context_compression.summary_model` is a bare
    /// model id that names no provider of its own — it would need to be
    /// resolved onto each consuming agent's OWN provider were the field ever
    /// read again — so when a single profile is shared by agents resolving
    /// to MORE THAN ONE distinct provider, that one bare id is ambiguous for
    /// at least one of them. A `summary_provider` supplies provider identity
    /// for this narrower diagnostic and excludes the corresponding value from
    /// the ambiguity count; it does not make context compression functional.
    ///
    /// The diagnostic is offline and deterministic: no schema bump, no
    /// network, and no model catalog. It names the profile, the affected
    /// agents, and their differing providers, then recommends removing the
    /// unsupported setting or waiting for an accepted compression design.
    fn collect_cross_provider_summary_model_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        for (profile_alias, profile) in &self.runtime_profiles {
            // Only the deprecated bare summary_model lacks provider identity.
            // A profile-level summary_provider excludes this narrower
            // ambiguity shape, but context compression remains inert.
            if !profile
                .context_compression
                .summary_provider
                .trim()
                .is_empty()
            {
                continue;
            }
            let Some(summary_model) = profile.context_compression.summary_model.as_deref() else {
                continue;
            };
            if summary_model.trim().is_empty() {
                continue;
            }

            // Gather agents that reference this profile and have no agent-level
            // summary_provider identity. An override excludes that agent from
            // this ambiguity diagnostic but does not make compression
            // functional. Resolve the provider that would be paired with the
            // bare model if a future implementation consumed this config.
            let mut affected: Vec<(String, String)> = Vec::new();
            for (agent_alias, agent) in &self.agents {
                if agent.runtime_profile.trim() != profile_alias {
                    continue;
                }
                if !agent.summary_provider.trim().is_empty() {
                    continue;
                }
                let provider_label = self.canonical_provider_label(agent.model_provider.trim());
                affected.push((agent_alias.clone(), provider_label));
            }

            // Cross-provider ambiguity requires distinct providers. A
            // same-provider bare id is still inert, but the generic
            // context-compression warning reports that fact without this
            // additional ambiguity detail.
            let distinct: std::collections::BTreeSet<&str> =
                affected.iter().map(|(_, p)| p.as_str()).collect();
            if distinct.len() < 2 {
                continue;
            }

            let mut agents_sorted: Vec<&(String, String)> = affected.iter().collect();
            agents_sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let detail = agents_sorted
                .iter()
                .map(|(name, provider)| format!("{name} -> {provider}"))
                .collect::<Vec<_>>()
                .join(", ");

            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "cross_provider_summary_model",
                format!(
                    "runtime_profiles.{profile_alias}.context_compression.summary_model \
                     ({summary_model:?}) is set, but context compression is not currently \
                     implemented in the runtime; this setting has no effect. It is also a \
                     bare model id reused by agents resolving to different providers \
                     ({detail}), which names no provider of its own and would be ambiguous \
                     for at least one of them if compression were read again. Remove the \
                     unsupported context_compression setting (every context_compression \
                     field, including summary_provider, is currently inert), or wait for a \
                     separately accepted compression design before configuring it."
                ),
                format!("runtime_profiles.{profile_alias}.context_compression.summary_model"),
            ));
        }
    }

    /// Surface every non-default `context_compression` knob as inert: the
    /// runtime context compressor was removed and nothing in the
    /// workspace reads `context_compression` at runtime anymore, so the whole
    /// struct — `enabled`, thresholds, protected counts, summarizer limits,
    /// provider selection, tool-result retrimming — has no effect. Mirrors
    /// `validate_memory_semantics`: one warning per non-default field, so a
    /// user sees exactly which of their authored knobs are dead. A field
    /// explicitly written at its default value is indistinguishable from an
    /// omitted one post-deserialization and stays silent (same limitation as
    /// `validate_memory_semantics`).
    ///
    /// Only `[runtime_profiles.<alias>.context_compression]` is checked —
    /// `AliasedAgentConfig` (`[agents.<alias>]`) has no `context_compression`
    /// field of its own. Current input is checked once per runtime profile.
    fn collect_context_compression_ignored_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        let defaults = crate::scattered_types::ContextCompressionConfig::default();
        for (alias, profile) in &self.runtime_profiles {
            let cc = &profile.context_compression;

            // `enabled = true` gets its own message: it is the master switch
            // users flip expecting compression to happen at all.
            if cc.enabled {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "context_compression_unsupported",
                    format!(
                        "runtime_profiles.{alias}.context_compression.enabled is set but context \
                         compression is not currently implemented in the runtime (the compressor \
                         was removed in #8196); this setting has no effect."
                    ),
                    format!("runtime_profiles.{alias}.context_compression.enabled"),
                ));
            }

            // Every other knob: flag any value that differs from the default.
            let mut inert: Vec<&'static str> = Vec::new();
            if (cc.threshold_ratio - defaults.threshold_ratio).abs() > f64::EPSILON {
                inert.push("threshold_ratio");
            }
            if cc.protect_first_n != defaults.protect_first_n {
                inert.push("protect_first_n");
            }
            if cc.protect_last_n != defaults.protect_last_n {
                inert.push("protect_last_n");
            }
            if cc.max_passes != defaults.max_passes {
                inert.push("max_passes");
            }
            if cc.summary_max_chars != defaults.summary_max_chars {
                inert.push("summary_max_chars");
            }
            if cc.source_max_chars != defaults.source_max_chars {
                inert.push("source_max_chars");
            }
            if cc.timeout_secs != defaults.timeout_secs {
                inert.push("timeout_secs");
            }
            if cc.summary_provider != defaults.summary_provider {
                inert.push("summary_provider");
            }
            if cc.summary_model != defaults.summary_model {
                // Ordering dependency: `collect_warnings()` runs
                // `collect_cross_provider_summary_model_warnings` before this
                // helper, so a cross-provider `summary_model` diagnostic for
                // this same path is already in `warnings`. That diagnostic
                // reports the same "has no effect" fact plus the specific
                // cross-provider agents affected, so it wins and the generic
                // inert warning is skipped to avoid printing two warnings
                // for the same config line. All other `summary_model`
                // shapes (single-provider, unshared) still get the inert
                // warning; no other diagnostic covers them.
                let summary_model_path =
                    format!("runtime_profiles.{alias}.context_compression.summary_model");
                if !warnings.iter().any(|w| {
                    w.code == "cross_provider_summary_model" && w.path == summary_model_path
                }) {
                    inert.push("summary_model");
                }
            }
            if cc.identifier_policy != defaults.identifier_policy {
                inert.push("identifier_policy");
            }
            if cc.tool_result_retrim_chars != defaults.tool_result_retrim_chars {
                inert.push("tool_result_retrim_chars");
            }
            if cc.tool_result_trim_exempt != defaults.tool_result_trim_exempt {
                inert.push("tool_result_trim_exempt");
            }

            for field in inert {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "context_compression_unsupported",
                    format!(
                        "runtime_profiles.{alias}.context_compression.{field} is set to a \
                         non-default value but context compression is not currently implemented \
                         in the runtime (the compressor was removed in #8196); this setting has \
                         no effect."
                    ),
                    format!("runtime_profiles.{alias}.context_compression.{field}"),
                ));
            }
        }
    }

    /// Surface every authored `[security.otp]` action-gating knob as
    /// deprecated and unsupported: ZeroClaw has no OTP action-gating, so
    /// `gated_actions`, `gated_domains`, `gated_domain_categories`, and
    /// `challenge_max_attempts` have no runtime consumer and provide no
    /// authorization boundary. OTP itself remains a live authentication
    /// mechanism — `enabled`, `token_ttl_secs`, and `cache_valid_secs`
    /// drive TOTP validation, the replay-cache window, and the e-stop
    /// resume challenge (`method` is parsed for forward compatibility;
    /// only TOTP is implemented today) — and authorization for high-risk
    /// actions is owned by the Tachi approval/grant plane with Node-local
    /// enforcement, not by OTP config.
    ///
    /// Mirrors `collect_context_compression_ignored_warnings`: one warning
    /// per non-default field, so a user sees exactly which of their
    /// authored knobs are dead. A field explicitly written at its default
    /// value is indistinguishable from an omitted one post-deserialization
    /// and stays silent (same limitation as that collector).
    fn collect_deprecated_otp_action_gating_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        let otp = &self.security.otp;
        let deprecated = "is deprecated and unsupported: ZeroClaw has no OTP action-gating, so it \
                          is parsed for backward compatibility but not enforced and provides no \
                          authorization boundary. High-risk action authorization is owned by the \
                          Tachi approval/grant plane with Node-local enforcement. Remove the \
                          setting.";
        if otp.gated_actions != default_otp_gated_actions() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.gated_actions {deprecated}"),
                "security.otp.gated_actions",
            ));
        }
        if !otp.gated_domains.is_empty() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.gated_domains {deprecated}"),
                "security.otp.gated_domains",
            ));
        }
        if !otp.gated_domain_categories.is_empty() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.gated_domain_categories {deprecated}"),
                "security.otp.gated_domain_categories",
            ));
        }
        if otp.challenge_max_attempts != default_otp_challenge_max_attempts() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.challenge_max_attempts {deprecated}"),
                "security.otp.challenge_max_attempts",
            ));
        }
    }

    /// Canonical label for an agent's resolved model provider, used to decide
    /// whether two agents sit on distinct providers. A non-empty ref that
    /// resolves through `[providers.models]` collapses to its canonical
    /// `<family>.<alias>` so equivalent spellings (bare vs dotted) compare
    /// equal; an empty or unresolved ref keeps its raw form (empty becomes a
    /// stable sentinel) so it still participates as a distinct bucket.
    fn canonical_provider_label(&self, provider_ref: &str) -> String {
        if provider_ref.is_empty() {
            return "<agent default provider>".to_string();
        }
        match self.providers.models.find_by_name(provider_ref) {
            Some((family, alias, _)) => format!("{family}.{alias}"),
            None => provider_ref.to_string(),
        }
    }

    /// Surface non-fatal issues in per-alias `fallback` chains: dangling refs
    /// (a fallback naming an alias that is not configured) and cycles (a
    /// fallback path that loops back onto itself). Both are warn-and-skip — the
    /// chain still loads and runs with the offending edge pruned at build time.
    fn collect_fallback_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        for (family, alias, cfg) in self.providers.models.iter_entries() {
            self.collect_fallback_model_warnings(family, alias, cfg, warnings);
            if cfg.fallback.is_empty() {
                continue;
            }
            let root = format!("{family}.{alias}");
            let mut visited: Vec<String> = vec![root.clone()];
            self.walk_fallback(&root, &cfg.fallback, &mut visited, 1, warnings);
        }
    }

    /// Surface `fallback_models` entries the build path silently skips: blank
    /// entries and entries that duplicate the alias's primary `model`. The skip
    /// itself is safe, but without a warning an operator never learns that a
    /// listed fallback model is doing nothing.
    fn collect_fallback_model_warnings(
        &self,
        family: &str,
        alias: &str,
        cfg: &ModelProviderConfig,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        let Some(primary) = cfg.model.as_deref() else {
            return;
        };
        for (i, model) in cfg.fallback_models.iter().enumerate() {
            let path = format!("providers.models.{family}.{alias}.fallback_models[{i}]");
            if model.trim().is_empty() {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "empty_fallback_model",
                    format!(
                        "fallback_models entry {i} on {family}.{alias} is empty; \
                         it is skipped at runtime"
                    ),
                    path,
                ));
            } else if model == primary {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "fallback_model_duplicates_primary",
                    format!(
                        "fallback_models entry {model:?} on {family}.{alias} duplicates the \
                         primary model; it is skipped at runtime"
                    ),
                    path,
                ));
            }
        }
    }

    fn walk_fallback(
        &self,
        from: &str,
        refs: &[crate::providers::ModelProviderRef],
        visited: &mut Vec<String>,
        depth: usize,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        if depth > crate::providers::MAX_FALLBACK_DEPTH {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "max_fallback_depth_exceeded",
                format!(
                    "fallback chain from {from} exceeds the maximum depth of {}; \
                     deeper links are pruned at runtime",
                    crate::providers::MAX_FALLBACK_DEPTH
                ),
                format!("providers.models.{from}.fallback"),
            ));
            return;
        }
        for (i, fallback_ref) in refs.iter().enumerate() {
            let raw = fallback_ref.as_str().trim();
            if raw.is_empty() {
                continue;
            }
            let path = format!("providers.models.{from}.fallback[{i}]");
            let Some((family, alias, cfg)) = self.providers.models.find_by_name(raw) else {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "dangling_fallback_ref",
                    format!(
                        "fallback {raw:?} on {from} does not resolve to a configured \
                         providers.models entry; this fallback link is skipped at runtime"
                    ),
                    path,
                ));
                continue;
            };
            let resolved = format!("{family}.{alias}");
            if visited.iter().any(|v| v == &resolved) {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "fallback_cycle",
                    format!(
                        "fallback {raw:?} on {from} closes a cycle \
                         ({} -> {resolved}); the cycle edge is pruned at runtime",
                        visited.join(" -> ")
                    ),
                    path,
                ));
                continue;
            }
            visited.push(resolved.clone());
            self.walk_fallback(&resolved, &cfg.fallback, visited, depth + 1, warnings);
            visited.pop();
        }
    }

    /// Validate configuration values that would cause runtime failures.
    ///
    /// Called after TOML deserialization and env-override application to catch
    /// obviously invalid values early instead of failing at arbitrary runtime points.
    pub fn validate(&self) -> Result<()> {
        validate_memory_rerank_config(&self.memory)?;
        if let Err(reason) = self.companion_memory.validate() {
            validation_bail!(
                RequiredFieldEmpty,
                "companion_memory.owner.principal_id",
                "{reason}"
            );
        }
        if let Some(target) = &self.companion_memory.review_notification {
            if let Err(reason) = target.validate() {
                validation_bail!(
                    InvalidFormat,
                    "companion_memory.review_notification",
                    "{reason}"
                );
            }
            if !self.gateway.bridges.contains_key(&target.bridge) {
                validation_bail!(
                    InvalidFormat,
                    "companion_memory.review_notification.bridge",
                    "review notification bridge must be configured in gateway.bridges"
                );
            }
        }
        if let Some(target) = &self.tachi.result_notification {
            if let Err(reason) = target.validate() {
                validation_bail!(InvalidFormat, "tachi.result_notification", "{reason}");
            }
            if !self.gateway.bridges.contains_key(&target.bridge) {
                validation_bail!(
                    InvalidFormat,
                    "tachi.result_notification.bridge",
                    "delegation result notification bridge must be configured in gateway.bridges"
                );
            }
        }
        if let Err(reason) = crate::node_allowlist::validate_config(self) {
            validation_bail!(InvalidFormat, "allowed_tools", "{reason}");
        }

        // Tunnel — OpenVPN
        if self.tunnel.tunnel_provider.trim() == "openvpn" {
            let openvpn = self.tunnel.openvpn.as_ref().ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::zeroclaw_log::Action::Reject
                    )
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "tunnel.tunnel_provider='openvpn' rejected: [tunnel.openvpn] block missing"
                );
                anyhow::Error::msg("tunnel.tunnel_provider='openvpn' requires [tunnel.openvpn]")
            })?;

            if openvpn.config_file.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "tunnel.openvpn.config_file",
                    "tunnel.openvpn.config_file must not be empty"
                );
            }
            if openvpn.connect_timeout_secs == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    "tunnel.openvpn.connect_timeout_secs",
                    "tunnel.openvpn.connect_timeout_secs must be greater than 0"
                );
            }
        }

        // Reply-pacing bounds — both `reply_min_interval_secs` and
        // `reply_queue_depth_max` walk through one entry list so adding
        // a new paced channel only requires extending `reply_pacing_entries`.
        for (path_prefix, cfg) in self.reply_pacing_entries() {
            let secs = cfg.reply_min_interval_secs();
            if secs > REPLY_MIN_INTERVAL_MAX_SECS {
                let path = format!("{path_prefix}.reply_min_interval_secs");
                validation_bail!(
                    InvalidNumericRange,
                    path,
                    "{path} = {secs} is out of range; must be 0..={REPLY_MIN_INTERVAL_MAX_SECS}"
                );
            }
            let depth = cfg.reply_queue_depth_max();
            if depth > REPLY_QUEUE_DEPTH_CEILING {
                let path = format!("{path_prefix}.reply_queue_depth_max");
                validation_bail!(
                    InvalidNumericRange,
                    path,
                    "{path} = {depth} is out of range; must be 0..={REPLY_QUEUE_DEPTH_CEILING}"
                );
            }
        }

        for (alias, tg) in &self.channels.telegram {
            tg.validate_bot_token(alias)?;
            validate_http_base_url(
                &format!("channels.telegram.{alias}.api_base_url"),
                &tg.api_base_url,
            )?;
        }

        for name in self.http_request.secrets.keys() {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                validation_bail!(
                    InvalidFormat,
                    format!("http_request.secrets.{name}"),
                    "http_request.secrets key {name:?} must contain 1..=64 ASCII letters, numbers, underscores, or hyphens"
                );
            }
        }

        // Gateway
        if self.gateway.host.trim().is_empty() {
            validation_bail!(
                RequiredFieldEmpty,
                "gateway.host",
                "gateway.host must not be empty"
            );
        }
        if self.nodes.mdns.max_peers == 0 {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.max_peers",
                "nodes.mdns.max_peers must be greater than 0"
            );
        }
        if self.nodes.mdns.announce_interval_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.announce_interval_secs",
                "nodes.mdns.announce_interval_secs must be greater than 0"
            );
        }
        if self.nodes.mdns.peer_ttl_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.peer_ttl_secs",
                "nodes.mdns.peer_ttl_secs must be greater than 0"
            );
        }
        if self.nodes.mdns.peer_ttl_secs <= self.nodes.mdns.announce_interval_secs {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.peer_ttl_secs",
                "nodes.mdns.peer_ttl_secs must be greater than nodes.mdns.announce_interval_secs"
            );
        }
        if matches!(self.transcription.max_audio_bytes, Some(0)) {
            validation_bail!(
                InvalidNumericRange,
                "transcription.max_audio_bytes",
                "transcription.max_audio_bytes must be greater than zero"
            );
        }
        if self.channels.max_concurrent_per_channel == 0 {
            validation_bail!(
                InvalidNumericRange,
                "channels.max_concurrent_per_channel",
                "channels.max_concurrent_per_channel must be greater than 0"
            );
        }
        // Typed-memory producers are a Tachi-only slice: the enabled write
        // path stores through `store_with_options_and_agent`, and only the
        // Tachi backend persists the full typed StoreOptions surface. Any
        // other backend would hit the trait default's explicit failure deep
        // inside spawned background consolidation, so reject the
        // configuration at load instead.
        if self.memory.types.enabled || self.memory.consolidation_extract_facts {
            let flag_path = if self.memory.types.enabled {
                "memory.types.enabled"
            } else {
                "memory.consolidation_extract_facts"
            };
            // Mirror the runtime's backend classification
            // (`backend_kind_from_dotted`): trim, take the prefix before the
            // first dot, compare case-insensitively.
            let backend_kind = self
                .memory
                .backend
                .trim()
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if backend_kind != "tachi" {
                validation_bail!(
                    InvalidFormat,
                    flag_path,
                    "{flag_path} = true requires memory.backend = \"tachi\" (typed memory storage is Tachi-only), but memory.backend = {:?}",
                    self.memory.backend
                );
            }
            for (alias, agent) in &self.agents {
                if !matches!(
                    agent.memory.backend,
                    crate::multi_agent::MemoryBackendKind::Tachi
                ) {
                    let agent_backend = agent.memory.backend;
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.memory.backend"),
                        "{flag_path} = true requires every agent on the tachi memory backend (typed memory storage is Tachi-only), but agents.{alias}.memory.backend = {agent_backend:?}",
                    );
                }
            }
        }
        for (alias, agent) in &self.agents {
            if agent.precheck.timeout_secs == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    format!("agents.{alias}.precheck.timeout_secs"),
                    "agents.{alias}.precheck.timeout_secs must be greater than 0"
                );
            }
        }
        // Cards: a card carries an agent's voice, its tool grants, and its
        // MCP reach as one authored unit. Letting it sit alongside the
        // pointers/fields it supersedes would mean an agent's real
        // authority depends on a precedence rule, and the failure mode of a
        // forgotten precedence rule is an agent reaching further than its
        // author believed. Refuse the ambiguity.
        //
        // `mcp_bundles` is checked here too: `Config::resolved_agent_config`
        // has a carded agent's `cards[card].grants.mcp_bundles` REPLACE
        // `agents.<alias>.mcp_bundles` outright (not union), so a raw
        // `mcp_bundles` left on a carded agent is silently ignored — the
        // same forgotten-precedence hazard this check exists to kill.
        // Configs that set `mcp_bundles` on an agent as a pre-card
        // workaround must drop it once the agent is carded: the card is now
        // the sole authority for that agent's MCP reach.
        for (alias, agent) in &self.agents {
            let card = agent.card.as_str().trim();
            if card.is_empty() {
                continue;
            }
            let Some(definition) = self.cards.get(card) else {
                validation_bail!(
                    DanglingReference,
                    format!("agents.{alias}.card"),
                    "agents.{alias}.card = {card:?} but no [cards.{card}] entry is configured"
                );
            };
            if let Err(reason) = definition.validate() {
                validation_bail!(
                    InvalidFormat,
                    format!("cards.{card}.grants"),
                    "cards.{card}: {reason}"
                );
            }
            for (field, value) in [
                ("persona", agent.persona.as_str()),
                ("risk_profile", agent.risk_profile.as_str()),
            ] {
                if !value.trim().is_empty() {
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.{field}"),
                        "agents.{alias} sets both card = {card:?} and {field} = {value:?}; \
                         a card already carries both, so set the card alone or drop it"
                    );
                }
            }
            // `mcp_bundles` is a `Vec<String>`, not a `&str` field, so it
            // cannot join the loop above — same check, own arm.
            if !agent.mcp_bundles.is_empty() {
                validation_bail!(
                    InvalidFormat,
                    format!("agents.{alias}.mcp_bundles"),
                    "agents.{alias} sets both card = {card:?} and mcp_bundles = {:?}; \
                     the card already carries the agent's MCP reach, so set the card \
                     alone or drop mcp_bundles",
                    agent.mcp_bundles
                );
            }
        }
        // Heartbeat agent: when heartbeat is enabled, the agent field
        // must name a configured agent.
        if self.heartbeat.enabled {
            let hb_agent = self.heartbeat.agent.trim();
            if hb_agent.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "heartbeat.agent",
                    "heartbeat.agent must reference a configured agent when heartbeat.enabled = true"
                );
            }
            if !self.agents.contains_key(hb_agent) {
                validation_bail!(
                    DanglingReference,
                    "heartbeat.agent",
                    "heartbeat.agent = {hb_agent:?} but no [agents.{hb_agent}] entry is configured"
                );
            }
        }
        if let Some(ref prefix) = self.gateway.path_prefix {
            // Validate the raw value — no silent trimming so the stored
            // value is exactly what was validated.
            if !prefix.is_empty() {
                if !prefix.starts_with('/') {
                    validation_bail!(
                        InvalidFormat,
                        "gateway.path_prefix",
                        "gateway.path_prefix must start with '/'"
                    );
                }
                if prefix.ends_with('/') {
                    validation_bail!(
                        InvalidFormat,
                        "gateway.path_prefix",
                        "gateway.path_prefix must not end with '/' (including bare '/')"
                    );
                }
                // Reject characters unsafe for URL paths or HTML/JS injection.
                // Whitespace is intentionally excluded from the allowed set.
                if let Some(bad) = prefix.chars().find(|c| {
                    !matches!(c, '/' | '-' | '_' | '.' | '~'
                        | 'a'..='z' | 'A'..='Z' | '0'..='9'
                        | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '='
                        | ':' | '@')
                }) {
                    anyhow::bail!(
                        "gateway.path_prefix contains invalid character '{bad}'; \
                         only unreserved and sub-delim URI characters are allowed"
                    );
                }
            }
        }

        for hash in &self.gateway.operator.token_hashes {
            if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                validation_bail!(
                    InvalidFormat,
                    "gateway.operator.token_hashes",
                    "operator credentials must be 64-character hex SHA-256 hashes of independent bearer tokens"
                );
            }
        }

        for (name, bridge) in &self.gateway.bridges {
            if bridge.scoped() && !self.gateway.require_pairing {
                validation_bail!(
                    InvalidFormat,
                    "gateway.require_pairing",
                    "scoped bridges require authenticated owner access (require_pairing = true)"
                );
            }
            if let Some(device) = &bridge.device
                && (device.agent.is_empty()
                    || !self.agents.contains_key(&device.agent)
                    || device.operations.iter().any(|op| {
                        !crate::bridge_access::GatewayDeviceBridgeConfig::valid_operation(op)
                    }))
            {
                validation_bail!(
                    InvalidFormat,
                    &format!("gateway.bridges.{name}.device"),
                    "device bridge requires a configured agent and explicit chat, answer, cancel or tachi_start/tachi_status/tachi_result/tachi_cancel operations"
                );
            }
            if let Some(device) = &bridge.device {
                if (device.risk_profile.is_some()
                    || device.operations.iter().any(|op| op == "chat"))
                    && let Err(reason) = device.validated_profile(self)
                {
                    validation_bail!(
                        InvalidFormat,
                        format!("gateway.bridges.{name}.device.risk_profile"),
                        "gateway.bridges.{name}.device.risk_profile: {reason}"
                    );
                }
                for (other_name, other) in &self.gateway.bridges {
                    if other_name != name && crate::bridge_access::namespaces_overlap(bridge, other)
                    {
                        anyhow::bail!(
                            "device bridge {name} session namespace overlaps bridge {other_name}"
                        );
                    }
                }
            }
            if bridge.device.is_some() && bridge.owner.is_some() {
                anyhow::bail!("gateway.bridges.{name}: device and owner are mutually exclusive");
            }
            if let Some(owner) = &bridge.owner
                && (owner.agent.is_empty()
                    || !self.agents.contains_key(&owner.agent)
                    || owner
                        .operations
                        .iter()
                        .any(|op| !matches!(op.as_str(), "chat" | "cancel" | "approval_response")))
            {
                anyhow::bail!(
                    "gateway.bridges.{name}.owner requires a configured agent and operations from chat, cancel, approval_response"
                );
            }
            let hash = bridge.token_hash.trim();
            if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                validation_bail!(
                    InvalidFormat,
                    &format!("gateway.bridges.{name}.token_hash"),
                    "gateway.bridges.{name}.token_hash must be the 64-character hex SHA-256 \
                     of the bridge token (mint one with `zeroclaw gateway bridge add {name}`)"
                );
            }
        }

        // Skill bundles — directories must stay inside `<install>/shared/`
        // and no two bundles may resolve to the same directory. Default
        // directory and the rules themselves live in
        // [`crate::skill_bundles`] so the runtime SkillsService and this
        // validator share one implementation.
        if !self.skill_bundles.is_empty() {
            let install_root = self.install_root_dir();
            for alias in self.skill_bundles.keys() {
                let dir = crate::skill_bundles::resolve_directory(self, &install_root, alias)
                    .map_err(|e| {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                                ::zeroclaw_log::Action::Reject
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "skill_bundle": alias,
                                "error": format!("{}", e),
                            })),
                            "skill_bundles.<alias>.directory could not be resolved"
                        );
                        anyhow::Error::msg(e.to_string())
                    })?;
                if let Err(e) = crate::skill_bundles::validate_directory(&dir, &install_root) {
                    validation_bail!(
                        InvalidFormat,
                        format!("skill-bundles.{alias}.directory"),
                        "{e}"
                    );
                }
            }
            if let Err(e) = crate::skill_bundles::validate_uniqueness(self, &install_root) {
                validation_bail!(InvalidFormat, "skill_bundles", "{e}");
            }
        }

        // MCP bundles: resolution is secure by default, so a dangling
        // reference fails closed (the agent is granted fewer servers, never
        // more). Surface the misconfiguration as a warning so an operator is
        // not left wondering why an agent's MCP tools vanished, without
        // turning a typo into a hard startup failure.
        {
            // Operator UX: secure-by-default means an agent with no
            // `mcp_bundles` grant connects to ZERO MCP servers. When MCP is
            // enabled and `[[mcp.servers]]` is non-empty but no
            // `[mcp_bundles.*]` exists at all, every agent silently gets
            // nothing. That is the inverse of the original silent
            // no-op and surprises operators upgrading from <0.8.3. Warn
            // once at startup so this surfaces via `doctor` and the
            // standard startup warning stream.
            if self.mcp.enabled && !self.mcp.servers.is_empty() && self.mcp_bundles.is_empty() {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::zeroclaw_log::Action::Note,
                    )
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "mcp_servers_configured": self.mcp.servers.len(),
                    })),
                    "[[mcp.servers]] is configured but no [mcp_bundles.*] bundles exist; no agent will receive any MCP tools. Add a [mcp_bundles.<alias>] entry and reference it from agents.<alias>.mcp_bundles to grant access."
                );
            }

            let known_servers: std::collections::HashSet<&str> =
                self.mcp.servers.iter().map(|s| s.name.as_str()).collect();
            for (bundle_alias, bundle) in &self.mcp_bundles {
                for server in bundle.servers.iter().chain(bundle.exclude.iter()) {
                    if !known_servers.contains(server.as_str()) {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "mcp_bundle": bundle_alias,
                                "server": server,
                            })),
                            "mcp_bundles.<alias> references an MCP server name not in [mcp.servers]; it grants nothing"
                        );
                    }
                }
            }
            for agent_alias in self.agents.keys() {
                let Some(agent) = self.resolved_agent_config(agent_alias) else {
                    continue;
                };
                for bundle_alias in &agent.mcp_bundles {
                    if !self.mcp_bundles.contains_key(bundle_alias) {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "agent": agent_alias,
                                "mcp_bundle": bundle_alias,
                            })),
                            "agents.<alias>.mcp_bundles references an undefined [mcp_bundles.<alias>]; the agent is granted no servers from it"
                        );
                    }
                }
            }
        }

        // Validate every configured risk profile. Each profile stands on
        // its own — there is no "active" or "default" risk profile concept;
        // an agent's `risk_profile` field names exactly which one applies.
        let mut profile_aliases: Vec<&String> = self.risk_profiles.keys().collect();
        profile_aliases.sort();
        for profile_alias in profile_aliases {
            let profile = &self.risk_profiles[profile_alias];
            for (i, env_name) in profile.shell_env_passthrough.iter().enumerate() {
                if !is_valid_env_var_name(env_name) {
                    anyhow::bail!(
                        "risk_profiles.{profile_alias}.shell_env_passthrough[{i}] is invalid ({env_name}); expected [A-Za-z_][A-Za-z0-9_]*"
                    );
                }
            }
        }

        // Security OTP / estop
        if self.security.otp.challenge_max_attempts == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.challenge_max_attempts",
                "security.otp.challenge_max_attempts must be greater than 0"
            );
        }
        if self.security.otp.token_ttl_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.token_ttl_secs",
                "security.otp.token_ttl_secs must be greater than 0"
            );
        }
        if self.security.otp.cache_valid_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.cache_valid_secs",
                "security.otp.cache_valid_secs must be greater than 0"
            );
        }
        if self.security.otp.cache_valid_secs < self.security.otp.token_ttl_secs {
            anyhow::bail!(
                "security.otp.cache_valid_secs must be greater than or equal to security.otp.token_ttl_secs"
            );
        }
        if self.security.otp.challenge_max_attempts == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.challenge_max_attempts",
                "security.otp.challenge_max_attempts must be greater than 0"
            );
        }
        for (i, action) in self.security.otp.gated_actions.iter().enumerate() {
            let normalized = action.trim();
            if normalized.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("security.otp.gated_actions[{i}]"),
                    "security.otp.gated_actions[{i}] must not be empty"
                );
            }
            if !normalized
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                anyhow::bail!(
                    "security.otp.gated_actions[{i}] contains invalid characters: {normalized}"
                );
            }
            if !default_otp_gated_actions()
                .iter()
                .any(|known| known == normalized)
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::zeroclaw_log::Action::Note
                    )
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "action": normalized,
                        "known_actions": default_otp_gated_actions(),
                    })),
                    "security.otp.gated_actions is deprecated and never enforced (ZeroClaw has no \
                     OTP action-gating); entry does not match a known action name: "
                );
            }
        }
        DomainMatcher::new(
            &self.security.otp.gated_domains,
            &self.security.otp.gated_domain_categories,
        )
        .with_context(
            || "Invalid security.otp.gated_domains or security.otp.gated_domain_categories",
        )?;
        if self.security.estop.state_file.trim().is_empty() {
            validation_bail!(
                RequiredFieldEmpty,
                "security.estop.state_file",
                "security.estop.state_file must not be empty"
            );
        }
        if !(0.0..=1.0).contains(&self.security.leak_detection.sensitivity) {
            validation_bail!(
                InvalidNumericRange,
                "security.leak_detection.sensitivity",
                "security.leak_detection.sensitivity must be between 0.0 and 1.0"
            );
        }

        // Scheduler
        if self.scheduler.max_concurrent == 0 {
            validation_bail!(
                InvalidNumericRange,
                "scheduler.max_concurrent",
                "scheduler.max_concurrent must be greater than 0"
            );
        }
        if self.scheduler.max_tasks == 0 {
            validation_bail!(
                InvalidNumericRange,
                "scheduler.max_tasks",
                "scheduler.max_tasks must be greater than 0"
            );
        }

        // Model routes
        for (i, route) in self.model_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("model_routes[{i}].hint"),
                    "model_routes[{i}].hint must not be empty"
                );
            }
            let mp = route.model_provider.trim();
            if mp.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("model_routes[{i}].model_provider"),
                    "model_routes[{i}].model_provider must not be empty"
                );
            }
            // Route refs are dotted `<type>.<alias>` and must resolve to a
            // configured `[providers.models.<type>.<alias>]` entry. Unresolved
            // routes are dropped at runtime construction; rejecting them here
            // keeps that drift visible at config-load time.
            match mp.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    if self.providers.models.find(ty, inner).is_none() {
                        validation_bail!(
                            DanglingReference,
                            format!("model_routes[{i}].model_provider"),
                            "model_routes[{i}].model_provider = {mp:?} but providers.models.{ty}.{inner} is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("model_routes[{i}].model_provider"),
                    "model_routes[{i}].model_provider must be dotted form `<type>.<alias>` (got {mp:?})",
                ),
            }
            if route.model.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("model_routes[{i}].model"),
                    "model_routes[{i}].model must not be empty"
                );
            }
        }

        // Embedding routes
        for (i, route) in self.embedding_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("embedding_routes[{i}].hint"),
                    "embedding_routes[{i}].hint must not be empty"
                );
            }
            let mp = route.model_provider.trim();
            if mp.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("embedding_routes[{i}].model_provider"),
                    "embedding_routes[{i}].model_provider must not be empty"
                );
            }
            // Embedding routes resolve against the same model-provider map;
            // there is no separate `providers.embeddings` typed section.
            match mp.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    if self.providers.models.find(ty, inner).is_none() {
                        validation_bail!(
                            DanglingReference,
                            format!("embedding_routes[{i}].model_provider"),
                            "embedding_routes[{i}].model_provider = {mp:?} but providers.models.{ty}.{inner} is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("embedding_routes[{i}].model_provider"),
                    "embedding_routes[{i}].model_provider must be dotted form `<type>.<alias>` (got {mp:?})",
                ),
            }
            if route.model.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("embedding_routes[{i}].model"),
                    "embedding_routes[{i}].model must not be empty"
                );
            }
        }

        for (type_key, alias_key, profile) in self.providers.models.iter_entries() {
            let profile_name = format!("{type_key}.{alias_key}");

            let has_uri = profile
                .uri
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());

            // Typed provider entries may omit an explicit `uri` (the
            // model_provider factory resolves the
            // family's default endpoint via `ModelEndpoint`). An entry
            // with no identifying information at all is almost always an
            // in-progress quickstart state — the user picked the model
            // provider but hasn't filled anything in yet. Warn but don't
            // bail; the runtime falls back to family-default endpoint at
            // use time, and a chat against the unconfigured model
            // provider fails with a clear error then.
            let has_api_key = profile
                .api_key
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty());
            let has_model = profile
                .model
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty());
            if !has_uri && !has_api_key && !has_model {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(crate::schema::CONFIG_SCHEMA_LOG_TARGET, ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": profile_name, "profile_name": profile_name})), "providers.models. is empty (no uri / api_key / model). \
                     Skipping at runtime; run `zeroclaw quickstart` (or use the dashboard) \
                     to make this model_provider usable.");
                continue;
            }

            if let Some(uri) = profile.uri.as_deref().map(str::trim)
                && !uri.is_empty()
            {
                let parsed = url::Url::parse(uri).with_context(|| {
                    format!("providers.models.{profile_name}.uri is not a valid URL")
                })?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    anyhow::bail!("providers.models.{profile_name}.uri must use http/https");
                }
            }

            if let Some(temp) = profile.temperature {
                validate_temperature(temp).map_err(|e| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(
                            crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                            ::zeroclaw_log::Action::Reject
                        )
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "profile": profile_name,
                            "temperature": temp,
                            "error": format!("{}", e),
                        })),
                        "providers.models.<alias>.temperature rejected"
                    );
                    anyhow::Error::msg(format!("providers.models.{profile_name}.temperature: {e}"))
                })?;
            }

            for (key, value) in &profile.pricing {
                if value.is_nan() {
                    anyhow::bail!(
                        "providers.models.{profile_name}.pricing.{key}: value must not be NaN"
                    );
                }
                if *value < 0.0 {
                    anyhow::bail!(
                        "providers.models.{profile_name}.pricing.{key}: value must be >= 0.0 (got {value})"
                    );
                }
            }
        }

        // Non-fatal validation warnings: surfaced both via tracing (CLI sees
        // on stderr) and via Config::collect_warnings (gateway HTTP returns
        // structured to dashboard callers). Single source of truth lives in
        // collect_warnings; emit each one to tracing here so the existing
        // log behavior is preserved.
        for w in self.collect_warnings() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::zeroclaw_log::Action::Note
                )
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"path": w.path, "code": w.code})),
                &format!("{}", w.message)
            );
        }

        // Ollama cloud-routing safety checks
        for (alias, cfg) in &self.providers.models.ollama {
            let entry = &cfg.base;
            if !entry
                .model
                .as_deref()
                .is_some_and(|model| model.trim().ends_with(":cloud"))
            {
                continue;
            }

            if is_local_ollama_endpoint(entry.uri.as_deref()) {
                anyhow::bail!(
                    "providers.models.ollama.{alias}.model uses ':cloud', but uri is local or unset. Set uri to a remote Ollama endpoint (for example https://ollama.com)."
                );
            }
            if is_official_ollama_cloud_endpoint(entry.uri.as_deref())
                && !has_ollama_cloud_credential(entry.api_key.as_deref())
            {
                anyhow::bail!(
                    "providers.models.ollama.{alias}.model uses ':cloud', but no API key is configured. Set api_key on [providers.models.ollama.{alias}] (or via the schema-mirror grammar: ZEROCLAW_providers__models__ollama__{alias}__api_key=<value>)."
                );
            }
        }

        // MCP
        if self.mcp.enabled {
            validate_mcp_config(&self.mcp)?;
        }

        // Tachi delegation client
        if let Err((path, reason)) = self.tachi.validate() {
            validation_bail!(InvalidFormat, path, "{reason}");
        }

        // Proxy (delegate to existing validation)
        self.proxy.validate()?;

        // Skills — extra registries
        {
            let mut seen = std::collections::HashSet::new();
            for (i, reg) in self.skills.extra_registries.iter().enumerate() {
                if reg.name.trim().is_empty() {
                    anyhow::bail!("skills.extra_registries[{i}].name must not be empty");
                }
                if !ExternalRegistry::is_valid_name(&reg.name) {
                    anyhow::bail!(
                        "skills.extra_registries[{i}].name '{}' is invalid; use only lowercase ASCII letters, numbers, '-' or '_' so it can be addressed as registry:<name>/<skill>",
                        reg.name
                    );
                }
                if !seen.insert(reg.name.as_str()) {
                    anyhow::bail!("skills.extra_registries has duplicate name '{}'", reg.name);
                }
                if reg.url.trim().is_empty() {
                    anyhow::bail!(
                        "skills.extra_registries[{}].url must not be empty",
                        reg.name
                    );
                }
                if reg.kind != ExternalRegistryKind::Git {
                    anyhow::bail!(
                        "skills.extra_registries[{}].kind must be 'git' (got '{}'); other protocols are not yet supported",
                        reg.name,
                        reg.kind
                    );
                }
                match url::Url::parse(&reg.url) {
                    Ok(u) if matches!(u.scheme(), "http" | "https" | "file") => {}
                    Ok(u) => anyhow::bail!(
                        "skills.extra_registries[{}].url scheme '{}' is unsupported (use http, https, or file)",
                        reg.name,
                        u.scheme()
                    ),
                    Err(e) => anyhow::bail!(
                        "skills.extra_registries[{}].url is not a valid URL: {e}",
                        reg.name
                    ),
                }
            }
        }

        // Pinggy tunnel region — validate allowed values (case-insensitive, auto-lowercased at runtime).
        if let Some(ref pinggy) = self.tunnel.pinggy
            && let Some(ref region) = pinggy.region
        {
            let r = region.trim().to_ascii_lowercase();
            if !r.is_empty() && !matches!(r.as_str(), "us" | "eu" | "ap" | "br" | "au") {
                anyhow::bail!(
                    "tunnel.pinggy.region must be one of: us, eu, ap, br, au (or omitted for auto)"
                );
            }
        }

        // Nevis IAM — delegate to NevisConfig::validate() for field-level checks
        if let Err(msg) = self.security.nevis.validate() {
            anyhow::bail!("security.nevis: {msg}");
        }

        // Per-profile validation: the context-compression summarizer provider
        // ref must resolve to a configured `[providers.models.*]` alias.
        // Empty = inherit (valid). A shared profile fails loud at config time
        // instead of only when some agent using it compresses.
        let mut profile_aliases: Vec<&String> = self.runtime_profiles.keys().collect();
        profile_aliases.sort();
        for palias in profile_aliases {
            let value = self.runtime_profiles[palias]
                .context_compression
                .summary_provider
                .trim();
            if value.is_empty() {
                continue;
            }
            match value.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    let exists = self
                        .get_map_keys(&format!("providers.models.{ty}"))
                        .is_some_and(|keys| keys.iter().any(|k| k == inner));
                    if !exists {
                        validation_bail!(
                            DanglingReference,
                            format!(
                                "runtime_profiles.{palias}.context_compression.summary_provider"
                            ),
                            "runtime_profiles.{palias}.context_compression.summary_provider = {value:?} but providers.models.{ty}.{inner} is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("runtime_profiles.{palias}.context_compression.summary_provider"),
                    "runtime_profiles.{palias}.context_compression.summary_provider must be dotted form `<type>.<alias>` (got {value:?})",
                ),
            }
        }

        // Per-agent validation. Mandatory + alias-existence checks live
        // here so the gateway PATCH path returns structured per-field
        // errors and the frontend never owns this rule. Sorted iteration
        // keeps error ordering stable across runs.
        let mut agent_aliases: Vec<&String> = self.agents.keys().collect();
        agent_aliases.sort();
        for alias in agent_aliases {
            let agent = &self.agents[alias];

            // model_provider: mandatory, dotted `<type>.<inner>` ref into
            // model_providers.<type>.<inner>.
            let mp = agent.model_provider.trim();
            if mp.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("agents.{alias}.model_provider"),
                    "agents.{alias}.model_provider must reference a configured model model_provider (e.g. \"anthropic.default\")",
                );
            }
            match mp.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    if !crate::providers::ModelProviders::slot_names().contains(&ty) {
                        validation_bail!(
                            DanglingReference,
                            format!("agents.{alias}.model_provider"),
                            "agents.{alias}.model_provider = {mp:?} but {ty:?} is not a known provider family; check [providers.models.<family>.<alias>] in config.toml (valid families: `zeroclaw providers`)",
                        );
                    }
                    let exists = self
                        .get_map_keys(&format!("providers.models.{ty}"))
                        .is_some_and(|keys| keys.iter().any(|k| k == inner));
                    if !exists {
                        validation_bail!(
                            DanglingReference,
                            format!("agents.{alias}.model_provider"),
                            "agents.{alias}.model_provider = {mp:?} but [providers.models.{ty}.{inner}] is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("agents.{alias}.model_provider"),
                    "agents.{alias}.model_provider must be dotted form `<type>.<alias>` (got {mp:?})",
                ),
            }

            // channels: each entry is a dotted `<type>.<inner>` ref into
            // channels.<type>.<inner>. Empty list is valid (delegate-only agent).
            // Uses the schema-derived `get_map_keys` so new channel types
            // surface here automatically — no per-type match arm.
            for (i, ch) in agent.channels.iter().enumerate() {
                let trimmed = ch.trim();
                match trimmed.split_once('.') {
                    // Retired channel type: the section tombstone reports it;
                    // the leftover binding has nothing to resolve and must not
                    // fail config load.
                    Some((ty, _)) if crate::validation_warnings::is_retired_channel_type(ty) => {}
                    Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                        // `get_map_keys` stores section names using the raw
                        // field ident (snake), the same dotted form the
                        // operator sees in TOML (`voice_call`, `wecom_ws`).
                        // Look up verbatim.
                        let exists = self
                            .get_map_keys(&format!("channels.{ty}"))
                            .is_some_and(|keys| keys.iter().any(|k| k == inner));
                        if !exists {
                            validation_bail!(
                                DanglingReference,
                                format!("agents.{alias}.channels[{i}]"),
                                "agents.{alias}.channels[{i}] = {trimmed:?} but channels.{ty}.{inner} is not configured",
                            );
                        }
                    }
                    _ => validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.channels[{i}]"),
                        "agents.{alias}.channels[{i}] must be dotted form `<type>.<alias>` (got {trimmed:?})",
                    ),
                }
            }

            // Per-agent provider refs that resolve into the typed provider
            // sections. Empty = no preference for that category (no TTS / no
            // STT for this agent), which is valid. Non-empty values must
            // match a configured `[providers.<category>.<type>.<alias>]`
            // entry, fail loud with the dangling ref otherwise.
            // there is no global default-X-provider concept — every consumer
            // either picks a configured alias or opts out entirely.
            let typed_provider_refs: &[(&str, &str, &str)] = &[
                ("providers.tts", "tts_provider", agent.tts_provider.trim()),
                (
                    "providers.transcription",
                    "transcription_provider",
                    agent.transcription_provider.trim(),
                ),
                // New field:
                (
                    "providers.models",
                    "classifier_provider",
                    agent.classifier_provider.trim(),
                ),
                // Agent-level context-compression summarizer override.
                (
                    "providers.models",
                    "summary_provider",
                    agent.summary_provider.trim(),
                ),
                // Advisor model target (`advisor = "model:<type>.<alias>"`);
                // a harness target is refused just below.
                (
                    "providers.models",
                    "advisor",
                    agent
                        .advisor
                        .as_ref()
                        .and_then(crate::advisor::AdvisorTarget::model_ref)
                        .map_or("", |model| model.trim()),
                ),
            ];
            if let Some(crate::advisor::AdvisorTarget::Harness(name)) = &agent.advisor {
                validation_bail!(
                    ValidationFailed,
                    format!("agents.{alias}.advisor"),
                    "agents.{alias}.advisor = \"harness:{name}\": harness advisors are not supported yet (see #405); use `model:<type>.<alias>`",
                );
            }
            if agent.advisor_max_calls_per_turn == Some(0) {
                validation_bail!(
                    InvalidNumericRange,
                    format!("agents.{alias}.advisor_max_calls_per_turn"),
                    "agents.{alias}.advisor_max_calls_per_turn must be at least 1; unset `advisor` to disable consultations",
                );
            }
            for (section_prefix, field, value) in typed_provider_refs {
                if value.is_empty() {
                    continue;
                }
                match value.split_once('.') {
                    Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                        let exists = self
                            .get_map_keys(&format!("{section_prefix}.{ty}"))
                            .is_some_and(|keys| keys.iter().any(|k| k == inner));
                        if !exists {
                            validation_bail!(
                                DanglingReference,
                                format!("agents.{alias}.{field}"),
                                "agents.{alias}.{field} = {value:?} but {section_prefix}.{ty}.{inner} is not configured",
                            );
                        }
                    }
                    _ => validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.{field}"),
                        "agents.{alias}.{field} must be dotted form `<type>.<alias>` (got {value:?})",
                    ),
                }
            }

            // Bare-alias bundle refs. Tuple is (kebab section path, kebab
            // agent field name, value list). Both names use the schema's
            // kebab form: section name matches what `get_map_keys` expects
            // (macro converts snake→kebab via `snake_to_kebab` per
            // crates/zeroclaw-macros/src/lib.rs:1056); field name matches
            // what `prop_fields()` emits, so DanglingReference paths bind
            // directly to the right inline error in the dashboard form.
            let bare_multi: &[(&str, &str, &[String])] = &[
                ("skill_bundles", "skill_bundles", &agent.skill_bundles),
                ("mcp_bundles", "mcp_bundles", &agent.mcp_bundles),
            ];
            for (section, field, values) in bare_multi {
                for (i, key) in values.iter().enumerate() {
                    let trimmed = key.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let exists = self
                        .get_map_keys(section)
                        .is_some_and(|keys| keys.iter().any(|k| k == trimmed));
                    if !exists {
                        validation_bail!(
                            DanglingReference,
                            format!("agents.{alias}.{field}[{i}]"),
                            "agents.{alias}.{field}[{i}] = {trimmed:?} but {section}.{trimmed} is not configured",
                        );
                    }
                }
            }
            let bare_single: &[(&str, &str, &str)] = &[
                ("risk_profiles", "risk_profile", agent.risk_profile.as_str()),
                (
                    "runtime_profiles",
                    "runtime_profile",
                    agent.runtime_profile.as_str(),
                ),
            ];
            for (section, field, raw) in bare_single {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let exists = self
                    .get_map_keys(section)
                    .is_some_and(|keys| keys.iter().any(|k| k == trimmed));
                if !exists {
                    validation_bail!(
                        DanglingReference,
                        format!("agents.{alias}.{field}"),
                        "agents.{alias}.{field} = {trimmed:?} but {section}.{trimmed} is not configured",
                    );
                }
            }

            // risk_profile is mandatory for enabled agents — there is no
            // global fallback, so an enabled agent with no profile can't
            // gate its actions. Run this check last so the more specific
            // dangling/format errors above surface first.
            //
            // A carded agent satisfies this through its card, which carries a
            // profile of its own. The invariant is unchanged — every enabled
            // agent still has exactly one profile — only where it is written
            // moves.
            if agent.enabled && agent.risk_profile.trim().is_empty() {
                let card = agent.card.as_str().trim();
                let carded_profile = self
                    .cards
                    .get(card)
                    .map(|card| card.risk_profile.as_str().trim())
                    .filter(|profile| !profile.is_empty());
                if let Some(carded_profile) = carded_profile {
                    // A card profile is still only a reference; accepting a
                    // dangling alias makes the agent look gated while no
                    // policy exists to enforce at runtime.
                    if !self.risk_profiles.contains_key(carded_profile) {
                        validation_bail!(
                            DanglingReference,
                            format!("cards.{card}.risk_profile"),
                            "cards.{card}.risk_profile = {carded_profile:?} but no [risk_profiles.{carded_profile}] entry is configured",
                        );
                    }
                } else {
                    validation_bail!(
                        RequiredFieldEmpty,
                        format!("agents.{alias}.risk_profile"),
                        "agents.{alias}.risk_profile must reference a configured [risk_profiles.<alias>] entry",
                    );
                }
            }

            // workspace.access: keys must point at OTHER agents, never
            // self, and every target must be a configured agent.
            for (target, mode) in &agent.workspace.access {
                let target_str = target.as_str();
                if target_str == alias.as_str() {
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.workspace.access.{target_str}"),
                        "agents.{alias}.workspace.access.{target_str} = {mode:?} but {target_str} is this agent itself; an agent always has full access to its own workspace, so self-references in the cross-agent allowlist are not permitted",
                    );
                }
                if !self.agents.contains_key(target_str) {
                    validation_bail!(
                        DanglingReference,
                        format!("agents.{alias}.workspace.access.{target_str}"),
                        "agents.{alias}.workspace.access.{target_str} = {mode:?} but agents.{target_str} is not configured",
                    );
                }
            }

            // workspace.read_memory_from: every alias must exist as a
            // configured agent and must use the same MemoryBackendKind
            // as the declaring agent. Mismatched backends fail at
            // config load rather than producing a runtime error when
            // the per-agent memory plumbing consumes the allowlist.
            let agent_backend = agent.memory.backend;
            for (i, target) in agent.workspace.read_memory_from.iter().enumerate() {
                let target_str = target.as_str();
                if target_str == alias.as_str() {
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.workspace.read_memory_from[{i}]"),
                        "agents.{alias}.workspace.read_memory_from[{i}] = {target_str:?} but {target_str} is this agent itself; an agent always sees its own memory rows, so self-references in the cross-agent allowlist are not permitted",
                    );
                }
                let Some(target_agent) = self.agents.get(target_str) else {
                    validation_bail!(
                        DanglingReference,
                        format!("agents.{alias}.workspace.read_memory_from[{i}]"),
                        "agents.{alias}.workspace.read_memory_from[{i}] = {target_str:?} but agents.{target_str} is not configured",
                    );
                };
                if target_agent.memory.backend != agent_backend {
                    let target_backend = target_agent.memory.backend;
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.workspace.read_memory_from[{i}]"),
                        "agents.{alias}.workspace.read_memory_from[{i}] points at agents.{target_str} which uses memory backend {target_backend:?}, but agents.{alias} uses {agent_backend:?}; the allowlist must point at same-backend siblings only",
                    );
                }
            }
        }

        // Peer groups: every member alias must exist as a configured
        // agent, and the group's channel must be in each member's
        // channels list. Mutual opt-in resolution happens at runtime;
        // this cross-reference check keeps misconfigured group
        // members from looking like real peer relationships at load
        // time.
        let mut peer_group_names: Vec<&String> = self.peer_groups.keys().collect();
        peer_group_names.sort();
        for group_name in peer_group_names {
            let group = &self.peer_groups[group_name];
            let group_channel = group.channel.trim();
            if group_channel.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("peer_groups.{group_name}.channel"),
                    "peer_groups.{group_name}.channel must name a channel type (e.g. \"telegram\") or dotted alias (e.g. \"telegram.work\")",
                );
            }
            // `get_map_keys` stores section names using the raw field ident
            // (snake); look up the channel type verbatim.
            let (group_channel_type, group_channel_alias) = match group_channel.split_once('.') {
                Some((ty, al)) => (ty, Some(al)),
                None => (group_channel, None),
            };
            // Retired channel type: the section tombstone reports it; a
            // leftover group on it has nothing to resolve against.
            if crate::validation_warnings::is_retired_channel_type(group_channel_type) {
                continue;
            }
            let channel_aliases = self.get_map_keys(&format!("channels.{group_channel_type}"));
            if channel_aliases.is_none() {
                validation_bail!(
                    DanglingReference,
                    format!("peer_groups.{group_name}.channel"),
                    "peer_groups.{group_name}.channel = {group_channel:?} but no [channels.{group_channel_type}.*] block is configured",
                );
            }
            if let Some(alias) = group_channel_alias {
                let exists = channel_aliases
                    .as_ref()
                    .is_some_and(|keys| keys.iter().any(|k| k == alias));
                if !exists {
                    validation_bail!(
                        DanglingReference,
                        format!("peer_groups.{group_name}.channel"),
                        "peer_groups.{group_name}.channel = {group_channel:?} but [channels.{group_channel_type}.{alias}] is not configured",
                    );
                }
            }
            for (i, member) in group.agents.iter().enumerate() {
                let member_str = member.as_str();
                let Some(member_agent) = self.agents.get(member_str) else {
                    validation_bail!(
                        DanglingReference,
                        format!("peer_groups.{group_name}.agents[{i}]"),
                        "peer_groups.{group_name}.agents[{i}] = {member_str:?} but agents.{member_str} is not configured",
                    );
                };
                let has_channel_match = member_agent.channels.iter().any(|ch| {
                    let ch_str = ch.as_str();
                    match group_channel_alias {
                        Some(alias) => ch_str == format!("{group_channel_type}.{alias}"),
                        None => ch_str.starts_with(&format!("{group_channel_type}.")),
                    }
                });
                if !has_channel_match {
                    let needs_msg = match group_channel_alias {
                        Some(alias) => format!("entry for {group_channel_type}.{alias}"),
                        None => format!("entry of type {group_channel_type:?}"),
                    };
                    validation_bail!(
                        InvalidFormat,
                        format!("peer_groups.{group_name}.agents[{i}]"),
                        "peer_groups.{group_name}.agents[{i}] = {member_str:?} but agents.{member_str}.channels has no {needs_msg}",
                    );
                }
            }
        }

        Ok(())
    }
}
