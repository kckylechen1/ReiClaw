//! Model inference through the OpenAI Responses API.

pub mod dispatch;
pub mod model_pin;
pub mod multimodal;
pub mod openai;
pub mod reliable;
pub mod router;
pub(crate) mod stream_guard;
pub mod traits;
pub mod vision_override;

mod responses;

pub use dispatch::{ProviderDispatch, ProviderDispatchRef};
pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, ModelProvider,
    ProviderCapabilityError, ToolCall, ToolResultMessage,
};

use reiclaw_config::providers::{reject_responses_wire_api, require_openai_model_provider};
use reliable::{ReliableModelProvider, ReliableModelProviderEntry};

const MAX_API_ERROR_CHARS: usize = 500;

pub(crate) fn extract_endpoint_host(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
}

#[derive(Debug, Clone, Default)]
pub struct ModelProviderRuntimeOptions {
    pub provider_kind: Option<String>,
    pub provider_api_url: Option<String>,
    pub reasoning_effort: Option<String>,
    pub provider_timeout_secs: Option<u64>,
    pub extra_headers: std::collections::HashMap<String, String>,
    pub provider_max_tokens: Option<u32>,
    pub wire_api: Option<String>,
    pub vision: Option<bool>,
    pub tls_ca_cert_path: Option<String>,
}

pub fn model_provider_runtime_options_from_model_provider_entry(
    config: &reiclaw_config::schema::Config,
    entry: Option<&reiclaw_config::schema::ModelProviderConfig>,
) -> ModelProviderRuntimeOptions {
    ModelProviderRuntimeOptions {
        provider_kind: entry.and_then(|e| e.kind.clone()),
        provider_api_url: entry.and_then(|e| e.uri.clone()),
        reasoning_effort: config.runtime.reasoning_effort.clone(),
        provider_timeout_secs: entry.and_then(|e| e.timeout_secs),
        extra_headers: entry.map(|e| e.extra_headers.clone()).unwrap_or_default(),
        provider_max_tokens: entry.and_then(|e| e.max_tokens),
        wire_api: entry.and_then(|e| e.wire_api.map(|w| w.as_str().to_string())),
        vision: entry.and_then(|e| e.vision),
        tls_ca_cert_path: entry.and_then(|e| e.tls_ca_cert_path.clone()),
    }
}

pub fn provider_runtime_options_for_agent(
    config: &reiclaw_config::schema::Config,
    agent_alias: &str,
) -> ModelProviderRuntimeOptions {
    model_provider_runtime_options_from_model_provider_entry(
        config,
        config.model_provider_for_agent(agent_alias),
    )
}

pub fn provider_runtime_options_for_alias(
    config: &reiclaw_config::schema::Config,
    family: &str,
    alias: &str,
) -> ModelProviderRuntimeOptions {
    model_provider_runtime_options_from_model_provider_entry(
        config,
        config.providers.models.find(family, alias),
    )
}

/// Options to use when building a provider from a name that may be either
/// a bare family or a dotted alias. Dotted names yield alias-resolved
/// options; bare names inherit only provider-agnostic settings from
/// `fallback`.
pub fn options_for_provider_ref(
    config: &reiclaw_config::schema::Config,
    name: &str,
    fallback: &ModelProviderRuntimeOptions,
) -> ModelProviderRuntimeOptions {
    match name.split_once('.') {
        Some((family, alias)) => provider_runtime_options_for_alias(config, family, alias),
        None => {
            let mut options = fallback.clone();
            options.provider_kind = None;
            options.provider_api_url = None;
            // `vision` is provider-specific: a bare family ref must not inherit
            // the fallback provider's capability flag. Clearing it falls back to
            // the family default (or the choke point's own resolution).
            options.vision = None;
            options
        }
    }
}

fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')
}

fn token_end(input: &str, from: usize) -> usize {
    let mut end = from;
    for (i, c) in input[from..].char_indices() {
        if is_secret_char(c) {
            end = from + i + c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Remove complete query strings from HTTP(S) URLs embedded in error text.
///
/// Query-value punctuation cannot safely identify where a credential ends:
/// commas, apostrophes, and parentheses are all legal query data. Treat the
/// URL's entire non-whitespace query tail as sensitive instead. This also
/// covers credential parameter names that the sanitizer does not know about.
fn scrub_url_queries(input: &str) -> String {
    let lowercase = input.to_ascii_lowercase();
    let mut scrubbed = String::with_capacity(input.len());
    let mut cursor = 0;

    while cursor < input.len() {
        let remaining = &input[cursor..];
        let remaining_lowercase = &lowercase[cursor..];
        let next_http = remaining_lowercase.find("http://");
        let next_https = remaining_lowercase.find("https://");
        let Some(relative_url_start) = (match (next_http, next_https) {
            (Some(http), Some(https)) => Some(http.min(https)),
            (Some(http), None) => Some(http),
            (None, Some(https)) => Some(https),
            (None, None) => None,
        }) else {
            scrubbed.push_str(remaining);
            break;
        };
        let url_start = cursor + relative_url_start;
        scrubbed.push_str(&input[cursor..url_start]);

        let url_tail = &input[url_start..];
        let url_end = url_start + url_tail.find(char::is_whitespace).unwrap_or(url_tail.len());
        let url_token = &input[url_start..url_end];
        if let Some(query_start) = url_token.find('?') {
            scrubbed.push_str(&url_token[..query_start]);
        } else {
            scrubbed.push_str(url_token);
        }
        cursor = url_end;
    }

    scrubbed
}

/// Scrub known secret-like token prefixes from model_provider error strings.
/// Redacts tokens with prefixes like `sk-`, `xoxb-`, `xoxp-`, `ghp_`, `gho_`,
/// `ghu_`, `github_pat_`, and Google/Gemini `AIza` keys. Complete query strings
/// are removed from embedded HTTP(S) URLs because query parameters may carry
/// credentials under provider-specific names.
pub fn scrub_secret_patterns(input: &str) -> String {
    const PREFIXES: [&str; 8] = [
        "sk-",
        "xoxb-",
        "xoxp-",
        "ghp_",
        "gho_",
        "ghu_",
        "github_pat_",
        "AIza",
    ];

    let mut scrubbed = scrub_url_queries(input);

    for prefix in PREFIXES {
        let mut search_from = 0;
        while let Some(rel) = scrubbed[search_from..].find(prefix) {
            let start = search_from + rel;
            let content_start = start + prefix.len();
            let end = token_end(&scrubbed, content_start);

            // Bare prefixes like "sk-" should not stop future scans.
            if end == content_start {
                search_from = content_start;
                continue;
            }

            scrubbed.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        }
    }

    scrubbed
}

/// Sanitize API error text by scrubbing secrets and truncating length.
pub fn sanitize_api_error(input: &str) -> String {
    let scrubbed = scrub_secret_patterns(input);

    if scrubbed.chars().count() <= MAX_API_ERROR_CHARS {
        return scrubbed;
    }

    let mut end = MAX_API_ERROR_CHARS;
    while end > 0 && !scrubbed.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}...", &scrubbed[..end])
}

/// Format an error including its full source chain and sanitize the result.
pub fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut formatted = String::new();
    let _ = std::fmt::Write::write_fmt(&mut formatted, format_args!("{error}"));
    let mut current = error.source();
    while let Some(source) = current {
        let _ = std::fmt::Write::write_fmt(&mut formatted, format_args!(": {source}"));
        current = source.source();
    }
    sanitize_api_error(&formatted)
}

/// Build a sanitized model_provider error from a failed HTTP response.
pub async fn api_error(model_provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read model_provider error body>".to_string());
    let sanitized = sanitize_api_error(&body);
    ::reiclaw_log::record!(
        ERROR,
        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
            .with_outcome(::reiclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "model_provider": model_provider,
                "status": status.as_u16(),
                "body": sanitized,
            })),
        "providers: API error"
    );
    anyhow::Error::msg(format!(
        "{model_provider} API error ({status}): {sanitized}"
    ))
}

/// Resolve API key for a model_provider from config and environment variables.
/// Return the typed-alias `api_key` field, trimmed. Env-var overrides land on
/// the field at config-load via the `REI_*` schema-mirror grammar.
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

// Guard the official endpoint against recognizable credentials belonging to
// other vendors. Custom routers require their own explicit credential.
const RETIRED_KEY_PREFIXES: &[(&str, &str)] = &[
    ("sk-ant-", "anthropic"),
    ("sk-or-", "openrouter"),
    ("gsk_", "groq"),
    ("AIza", "gemini"),
    ("pplx-", "perplexity"),
    ("xai-", "xai"),
    ("nvapi-", "nvidia"),
    ("KEY-", "telnyx"),
];

fn validate_endpoint_credential(url: Option<&str>, key: Option<&str>) -> anyhow::Result<()> {
    if let Some(url) = url {
        openai::validate_endpoint(url)?;
    }
    if url.is_none_or(openai::is_official_endpoint)
        && let Some(key) = key
        && let Some((_, provider)) = RETIRED_KEY_PREFIXES
            .iter()
            .find(|(prefix, _)| key.starts_with(prefix))
    {
        anyhow::bail!(
            "The configured credential looks like a {provider} API key. Set an OpenAI API key for the official endpoint, or configure the router URL and its key on [providers.models.openai.<alias>]."
        );
    }
    Ok(())
}

/// Whether a configured alias has a usable endpoint and its required credential.
pub fn model_provider_endpoint_ready(entry: &reiclaw_config::schema::ModelProviderConfig) -> bool {
    let url = nonempty(entry.uri.as_deref());
    let explicit_key = nonempty(entry.api_key.as_deref());
    let ambient_key = if explicit_key.is_none() && url.is_none() {
        std::env::var("OPENAI_API_KEY").ok()
    } else {
        None
    };
    let key = explicit_key.or_else(|| nonempty(ambient_key.as_deref()));
    validate_endpoint_credential(url, key).is_ok()
        && (key.is_some() || url.is_some_and(|url| !openai::is_official_endpoint(url)))
}

pub fn create_model_provider(
    name: &str,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_model_provider_inner(
        None,
        name,
        "default",
        api_key,
        None,
        &ModelProviderRuntimeOptions::default(),
    )
}

/// Factory: create model_provider with runtime options.
/// Legacy entry point — see [`create_model_provider`].
pub fn create_model_provider_with_options(
    name: &str,
    api_key: Option<&str>,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_model_provider_inner(None, name, "default", api_key, None, options)
}

/// Factory: create model_provider with optional custom base URL.
/// Legacy entry point — see [`create_model_provider`].
pub fn create_model_provider_with_url(
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_model_provider_inner(
        None,
        name,
        "default",
        api_key,
        api_url,
        &ModelProviderRuntimeOptions::default(),
    )
}

pub fn create_model_provider_for_alias(
    config: &reiclaw_config::schema::Config,
    family: &str,
    alias: &str,
    api_key: Option<&str>,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_model_provider_inner(Some(config), family, alias, api_key, None, options)
}

/// Factory: create model_provider with alias context AND custom base URL.
pub fn create_model_provider_for_alias_with_url(
    config: &reiclaw_config::schema::Config,
    family: &str,
    alias: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_model_provider_inner(Some(config), family, alias, api_key, api_url, options)
}

/// Apply the config `vision` capability override to a freshly-constructed
/// provider. Called at every exit of `create_model_provider_inner`, the single
/// construction choke point every subsystem funnels through, so the override
/// lands once and `supports_vision()` stays consistent across the
/// vision-routing gate, the channel media pipeline, and the model router
/// without per-family or per-consumer re-derivation.
fn apply_vision_override(
    provider: Box<dyn ModelProvider>,
    vision: Option<bool>,
) -> Box<dyn ModelProvider> {
    match vision {
        Some(vision) => Box::new(vision_override::VisionOverrideProvider::new(
            provider, vision,
        )),
        None => provider,
    }
}

fn create_model_provider_inner(
    config: Option<&reiclaw_config::schema::Config>,
    name: &str,
    alias: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    require_openai_model_provider(name)?;
    if let Some(kind) = nonempty(options.provider_kind.as_deref()) {
        require_openai_model_provider(kind)?;
    }
    if let Some(wire) = options.wire_api.as_deref() {
        reject_responses_wire_api(wire)?;
    }
    let entry = config.and_then(|cfg| cfg.providers.models.find(name, alias));
    if let Some(entry) = entry {
        reiclaw_config::providers::validate_responses_entry(entry)?;
    }
    let url = nonempty(api_url)
        .or_else(|| nonempty(options.provider_api_url.as_deref()))
        .or_else(|| entry.and_then(|e| nonempty(e.uri.as_deref())));
    let explicit_key =
        nonempty(api_key).or_else(|| entry.and_then(|e| nonempty(e.api_key.as_deref())));
    let ambient_key = if explicit_key.is_none() && url.is_none() {
        std::env::var("OPENAI_API_KEY").ok()
    } else {
        None
    };
    let key = explicit_key.or_else(|| nonempty(ambient_key.as_deref()));
    validate_endpoint_credential(url, key)?;
    let mut builder = openai::OpenAiResponsesModelProvider::builder(alias)
        .credential(key)
        .max_tokens(options.provider_max_tokens)
        .reasoning_effort(options.reasoning_effort.clone())
        .extra_headers(options.extra_headers.clone());
    if let Some(url) = url {
        builder = builder.api_url(url);
    }
    if let Some(timeout) = options.provider_timeout_secs {
        builder = builder.timeout_secs(timeout);
    }
    if let Some(path) = options.tls_ca_cert_path.as_deref() {
        builder = builder.tls_ca_cert_path(path)?;
    }
    Ok(apply_vision_override(
        Box::new(builder.build()),
        options.vision,
    ))
}

pub fn create_resilient_model_provider_with_options(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &reiclaw_config::schema::ReliabilityConfig,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    let primary_model_provider =
        create_model_provider_inner(None, primary_name, "default", api_key, api_url, options)?;

    let reliable = ReliableModelProvider::new(
        primary_name,
        vec![(primary_name.to_string(), primary_model_provider)],
        reliability.provider_retries,
        reliability.provider_backoff_ms,
    )
    .with_api_keys(reliability.api_keys.clone());

    Ok(Box::new(reliable))
}

/// Wrap the primary model_provider in a retry/backoff harness with full
/// alias context. Production callers (gateway, orchestrator) use this so
/// the factory sees the endpoint and credential for the configured alias.
pub fn create_resilient_model_provider_for_alias(
    config: &reiclaw_config::schema::Config,
    family: &str,
    alias: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &reiclaw_config::schema::ReliabilityConfig,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_resilient_model_provider_for_alias_with_model_override(
        config,
        family,
        alias,
        api_key,
        api_url,
        reliability,
        options,
        None,
    )
}

fn create_resilient_model_provider_for_alias_with_model_override(
    config: &reiclaw_config::schema::Config,
    family: &str,
    alias: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &reiclaw_config::schema::ReliabilityConfig,
    options: &ModelProviderRuntimeOptions,
    primary_model_override: Option<&str>,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    let primary_model_provider =
        create_model_provider_inner(Some(config), family, alias, api_key, api_url, options)?;

    let mut model_providers: Vec<ReliableModelProviderEntry> = Vec::new();
    push_pinned_entries(
        &mut model_providers,
        config,
        family,
        alias,
        primary_model_provider,
        primary_model_override,
    );

    let mut visited: Vec<String> = vec![format!("{family}.{alias}")];
    if let Some(entry) = config.providers.models.find(family, alias) {
        append_fallback_chain(
            &mut model_providers,
            config,
            &entry.fallback,
            &mut visited,
            1,
        )?;
    }

    let reliable = ReliableModelProvider::new_with_entries(
        alias,
        model_providers,
        reliability.provider_retries,
        reliability.provider_backoff_ms,
    )
    .with_api_keys(reliability.api_keys.clone());

    Ok(Box::new(reliable))
}

fn push_pinned_entries(
    out: &mut Vec<ReliableModelProviderEntry>,
    config: &reiclaw_config::schema::Config,
    family: &str,
    alias: &str,
    built: Box<dyn ModelProvider>,
    primary_model_override: Option<&str>,
) {
    let entry = config.providers.models.find(family, alias);
    let primary_model = primary_model_override
        .and_then(|model| {
            let trimmed = model.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .or_else(|| entry.and_then(|e| e.model.as_deref()));
    let extra_models: &[String] = entry.map(|e| e.fallback_models.as_slice()).unwrap_or(&[]);
    let cooldown_key = format!("{family}.{alias}");

    let Some(primary_model) = primary_model else {
        out.push(ReliableModelProviderEntry::new(family, cooldown_key, built));
        return;
    };

    let built: std::sync::Arc<dyn ModelProvider> = std::sync::Arc::from(built);
    out.push(ReliableModelProviderEntry::new_pinned(
        family,
        cooldown_key.clone(),
        alias,
        primary_model,
        Box::new(std::sync::Arc::clone(&built)),
    ));
    for model in extra_models {
        if model.trim().is_empty() || model == primary_model {
            continue;
        }
        out.push(ReliableModelProviderEntry::new_pinned(
            family,
            cooldown_key.clone(),
            alias,
            model,
            Box::new(std::sync::Arc::clone(&built)),
        ));
    }
}

fn append_fallback_chain(
    out: &mut Vec<ReliableModelProviderEntry>,
    config: &reiclaw_config::schema::Config,
    refs: &[reiclaw_config::providers::ModelProviderRef],
    visited: &mut Vec<String>,
    depth: usize,
) -> anyhow::Result<()> {
    if depth > reiclaw_config::providers::MAX_FALLBACK_DEPTH {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                        "max_depth": reiclaw_config::providers::MAX_FALLBACK_DEPTH
                })),
            "fallback chain exceeds max depth; pruning"
        );
        return Ok(());
    }
    for fallback_ref in refs {
        let raw = fallback_ref.as_str().trim();
        reiclaw_config::providers::reject_retired_model_provider(raw)?;
        if raw.is_empty() {
            continue;
        }
        let Some((family, alias, entry)) = config.providers.models.find_by_name(raw) else {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"fallback": raw})),
                "fallback ref does not resolve; skipping"
            );
            continue;
        };
        let resolved = format!("{family}.{alias}");
        if visited.iter().any(|v| v == &resolved) {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"fallback": resolved})),
                "fallback ref closes a cycle; pruning"
            );
            continue;
        }

        let opts = provider_runtime_options_for_alias(config, family, &alias);
        if nonempty(entry.api_key.as_deref()).is_none() {
            let profile = format!("[providers.models.{family}.{alias}]");
            anyhow::bail!(
                "Fallback provider `{raw}` resolved to `{resolved}` ({profile}) but no                  profile-resolved credential exists. Set `api_key` on {profile} or remove it from `fallback`."
            );
        }

        match create_model_provider_inner(
            Some(config),
            family,
            &alias,
            entry.api_key.as_deref(),
            entry.uri.as_deref(),
            &opts,
        ) {
            Ok(built) => push_pinned_entries(out, config, family, &alias, built, None),
            Err(e) => {
                let profile = format!("[providers.models.{family}.{alias}]");
                anyhow::bail!(
                    "Fallback provider `{raw}` resolved to `{resolved}` ({profile}) failed to \
                     build: {e}"
                );
            }
        }

        visited.push(resolved.clone());
        append_fallback_chain(out, config, &entry.fallback, visited, depth + 1)?;
        visited.pop();
    }
    Ok(())
}

pub fn create_resilient_model_provider_from_ref(
    config: &reiclaw_config::schema::Config,
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &reiclaw_config::schema::ReliabilityConfig,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_resilient_model_provider_from_ref_with_model_override(
        config,
        name,
        api_key,
        api_url,
        reliability,
        options,
        None,
    )
}

/// A provider resolved from a config reference together with the model declared
/// by that same alias. The model is an on-demand view of Config, not stored
/// provider state.
pub struct ResolvedModelProviderRef {
    pub provider: Box<dyn ModelProvider>,
    pub model: Option<String>,
}

/// Build a non-resilient provider by family (`"openai"`) or configured alias
/// (`"openai.text_model"`), resolving alias-specific options from `config`: the
/// `vision` capability override, the endpoint URI, and per-alias credentials
/// from `[providers.models.<family>.<alias>]`.
///
/// Unlike the legacy `create_model_provider(name, None)` (which passes
/// `config = None` and `ModelProviderRuntimeOptions::default()`, so it cannot see
/// any per-alias config), this honors the alias's `vision` override and typed
/// config while returning an un-wrapped provider - the shape the dedicated
/// vision route needs so its configured `[multimodal] vision_model_provider`
/// respects a per-alias `vision = true/false`.
pub fn create_model_provider_from_ref(
    config: &reiclaw_config::schema::Config,
    name: &str,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    create_model_provider_from_ref_with_model(config, name).map(|resolved| resolved.provider)
}

/// Resolve a provider reference and its alias-owned model as one materialized
/// view. Callers may apply their own explicit model override after resolution;
/// when the reference is bare, `model` is `None`.
pub fn create_model_provider_from_ref_with_model(
    config: &reiclaw_config::schema::Config,
    name: &str,
) -> anyhow::Result<ResolvedModelProviderRef> {
    // Resolve an explicitly configured alias or let the bare-family factory
    // reject the intact reference. A missing alias must never use the default endpoint.
    if let Some((family, alias)) = name.split_once('.')
        && let Some(entry) = config.providers.models.find(family, alias)
    {
        let options = provider_runtime_options_for_alias(config, family, alias);
        let provider = create_model_provider_inner(
            Some(config),
            family,
            alias,
            entry.api_key.as_deref(),
            entry.uri.as_deref(),
            &options,
        )?;
        let model = entry
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(ToString::to_string);
        return Ok(ResolvedModelProviderRef { provider, model });
    }
    let provider = create_model_provider_inner(
        None,
        name,
        "default",
        None,
        None,
        &ModelProviderRuntimeOptions::default(),
    )?;
    Ok(ResolvedModelProviderRef {
        provider,
        model: None,
    })
}

fn create_resilient_model_provider_from_ref_with_model_override(
    config: &reiclaw_config::schema::Config,
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &reiclaw_config::schema::ReliabilityConfig,
    options: &ModelProviderRuntimeOptions,
    primary_model_override: Option<&str>,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    reiclaw_config::providers::reject_retired_model_provider(name)?;
    if let Some((family, alias)) = name.split_once('.') {
        require_openai_model_provider(family)?;
        anyhow::ensure!(
            config.providers.models.find(family, alias).is_some(),
            "Model provider alias is not configured. Add it under [providers.models.openai.<alias>]."
        );
    }
    match name.split_once('.') {
        Some((family, alias)) => create_resilient_model_provider_for_alias_with_model_override(
            config,
            family,
            alias,
            api_key,
            api_url,
            reliability,
            options,
            primary_model_override,
        ),
        None => create_resilient_model_provider_with_options(
            name,
            api_key,
            api_url,
            reliability,
            options,
        ),
    }
}

pub fn create_routed_model_provider_with_options(
    config: &reiclaw_config::schema::Config,
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &reiclaw_config::schema::ReliabilityConfig,
    model_routes: &[reiclaw_config::schema::ModelRouteConfig],
    default_model: &str,
    options: &ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    if model_routes.is_empty() {
        return create_resilient_model_provider_from_ref_with_model_override(
            config,
            primary_name,
            api_key,
            api_url,
            reliability,
            options,
            Some(default_model),
        );
    }

    // Collect unique model_provider names needed
    let mut needed: Vec<String> = vec![primary_name.to_string()];
    for route in model_routes {
        if !needed.iter().any(|n| n == &route.model_provider) {
            needed.push(route.model_provider.clone());
        }
    }

    // Create each model_provider (with its own resilience wrapper). Each
    // entry's options come from its own typed alias block when dotted;
    // the primary inherits the caller's options (already alias-resolved
    // upstream for the owning agent).
    let mut model_providers: Vec<(String, Box<dyn ModelProvider>)> = Vec::new();
    for name in &needed {
        reiclaw_config::providers::reject_retired_model_provider(name)?;
        let is_primary = name == primary_name;
        let routed_credential = model_routes
            .iter()
            .find(|r| &r.model_provider == name)
            .and_then(|r| {
                r.api_key.as_ref().and_then(|raw_key| {
                    let trimmed_key = raw_key.trim();
                    (!trimmed_key.is_empty()).then_some(trimmed_key)
                })
            });
        let key = routed_credential
            .or_else(|| {
                name.split_once('.')
                    .and_then(|(family, alias)| {
                        config
                            .providers
                            .models
                            .find(family, alias)
                            .and_then(|cfg| cfg.api_key.as_deref())
                    })
                    .and_then(|raw_key| {
                        let trimmed = raw_key.trim();
                        (!trimmed.is_empty()).then_some(trimmed)
                    })
            })
            .or_else(|| is_primary.then_some(api_key).flatten());
        let url = if is_primary { api_url } else { None };
        let entry_options = if is_primary {
            options.clone()
        } else {
            options_for_provider_ref(config, name, options)
        };

        match create_resilient_model_provider_from_ref_with_model_override(
            config,
            name,
            key,
            url,
            reliability,
            &entry_options,
            is_primary.then_some(default_model),
        ) {
            Ok(model_provider) => model_providers.push((name.clone(), model_provider)),
            Err(e) => {
                anyhow::bail!("Routed model_provider `{name}` failed to initialize: {e}");
            }
        }
    }

    // Build route table
    let routes: Vec<(String, router::Route)> = model_routes
        .iter()
        .map(|r| {
            (
                r.hint.clone(),
                router::Route {
                    provider_name: r.model_provider.clone(),
                    model: r.model.clone(),
                },
            )
        })
        .collect();

    Ok(Box::new(router::RouterModelProvider::new(
        primary_name,
        model_providers,
        routes,
    )))
}

/// A built-in model provider available to setup and provider listing.
pub struct ModelProviderInfo {
    pub name: &'static str,
    pub display_name: &'static str,
    pub local: bool,
    pub category: ModelProviderCategory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProviderCategory {
    Primary,
}

impl ModelProviderCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        "Primary"
    }

    #[must_use]
    pub fn all() -> &'static [Self] {
        &[Self::Primary]
    }
}

pub fn list_model_providers() -> Vec<ModelProviderInfo> {
    vec![ModelProviderInfo {
        name: "openai",
        display_name: "OpenAI Responses",
        local: false,
        category: ModelProviderCategory::Primary,
    }]
}

#[must_use]
pub fn canonical_model_provider_slots() -> Vec<&'static str> {
    reiclaw_config::providers::ModelProviders::slot_names().to_vec()
}

/// Look up optional context-window metadata from the configured Responses endpoint.
pub async fn fetch_context_window(
    provider_type: &str,
    config: &reiclaw_config::schema::ModelProviderConfig,
) -> Option<usize> {
    require_openai_model_provider(provider_type).ok()?;
    reiclaw_config::providers::validate_responses_entry(config).ok()?;
    let key = nonempty(config.api_key.as_deref())?;
    if reiclaw_config::secrets::SecretStore::is_encrypted(key) {
        return None;
    }
    let url = nonempty(config.uri.as_deref());
    validate_endpoint_credential(url, Some(key)).ok()?;
    let mut builder = openai::OpenAiResponsesModelProvider::builder("context-window")
        .credential(Some(key))
        .extra_headers(config.extra_headers.clone());
    if let Some(url) = url {
        builder = builder.api_url(url);
    }
    if let Some(path) = config.tls_ca_cert_path.as_deref() {
        builder = builder.tls_ca_cert_path(path).ok()?;
    }
    builder
        .build()
        .context_window(config.model.as_deref()?)
        .await
}

#[cfg(test)]
mod tests;
