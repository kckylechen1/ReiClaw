//! Shared outbound HTTP clients and process proxy application.
//!
//! `zeroclaw-config` owns the persisted proxy schema and validation. This crate
//! owns its runtime application and disposable connection cache; callers refresh
//! that projection from the loaded config at startup and reload.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use zeroclaw_config::schema::{ProxyConfig, ProxyScope, normalize_proxy_url_option};

static RUNTIME_PROXY_STATE: OnceLock<RwLock<RuntimeProxyState>> = OnceLock::new();

#[derive(Default)]
struct RuntimeProxyState {
    config: ProxyConfig,
    generation: u64,
    clients: HashMap<String, reqwest::Client>,
}

fn apply_proxy_to_builder(
    config: &ProxyConfig,
    mut builder: reqwest::ClientBuilder,
    service_key: &str,
) -> reqwest::ClientBuilder {
    if !config.should_apply_to_service(service_key) {
        return builder;
    }

    let no_proxy = no_proxy_value(config);

    if let Some(url) = normalize_proxy_url_option(config.all_proxy.as_deref()) {
        match reqwest::Proxy::all(&url) {
            Ok(proxy) => {
                builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
            }
            Err(error) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid all_proxy URL: ");
            }
        }
    }

    if let Some(url) = normalize_proxy_url_option(config.http_proxy.as_deref()) {
        match reqwest::Proxy::http(&url) {
            Ok(proxy) => {
                builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
            }
            Err(error) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid http_proxy URL: ");
            }
        }
    }

    if let Some(url) = normalize_proxy_url_option(config.https_proxy.as_deref()) {
        match reqwest::Proxy::https(&url) {
            Ok(proxy) => {
                builder = builder.proxy(apply_no_proxy(proxy, no_proxy));
            }
            Err(error) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid https_proxy URL: ");
            }
        }
    }

    builder
}

fn no_proxy_value(config: &ProxyConfig) -> Option<reqwest::NoProxy> {
    let joined = {
        let list = config.normalized_no_proxy();
        (!list.is_empty()).then(|| list.join(","))
    };
    joined.as_deref().and_then(reqwest::NoProxy::from_string)
}
fn apply_no_proxy(proxy: reqwest::Proxy, no_proxy: Option<reqwest::NoProxy>) -> reqwest::Proxy {
    proxy.no_proxy(no_proxy)
}
fn apply_to_process_env(config: &ProxyConfig) {
    set_proxy_env_pair("HTTP_PROXY", config.http_proxy.as_deref());
    set_proxy_env_pair("HTTPS_PROXY", config.https_proxy.as_deref());
    set_proxy_env_pair("ALL_PROXY", config.all_proxy.as_deref());

    let no_proxy_joined = {
        let list = config.normalized_no_proxy();
        (!list.is_empty()).then(|| list.join(","))
    };
    set_proxy_env_pair("NO_PROXY", no_proxy_joined.as_deref());
}

fn set_proxy_env_pair(key: &str, value: Option<&str>) {
    let lowercase_key = key.to_ascii_lowercase();
    if let Some(value) = value.and_then(|candidate| normalize_proxy_url_option(Some(candidate))) {
        // SAFETY: called during single-threaded config init before async runtime starts.
        unsafe {
            std::env::set_var(key, &value);
            std::env::set_var(lowercase_key, value);
        }
    } else {
        // SAFETY: called during single-threaded config init before async runtime starts.
        unsafe {
            std::env::remove_var(key);
            std::env::remove_var(lowercase_key);
        }
    }
}

fn runtime_proxy_state() -> &'static RwLock<RuntimeProxyState> {
    RUNTIME_PROXY_STATE.get_or_init(|| RwLock::new(RuntimeProxyState::default()))
}

fn runtime_proxy_cache_key(
    service_key: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> String {
    format!(
        "{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string())
    )
}

/// Build from one config snapshot without holding the runtime lock during client
/// construction. A reload invalidates both the cache and any pending insertion.
/// Adapted from upstream cda933e6b9937deacbcc0d4d2cd7f1ae5bb4500e; this fork keeps
/// config, generation and cache together in its HTTP runtime owner.
fn build_cached_proxy_client(
    cache_key: String,
    build: impl FnOnce(&ProxyConfig) -> reqwest::Client,
) -> reqwest::Client {
    let (generation, config) = {
        let state = runtime_proxy_state()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = state.clients.get(&cache_key) {
            return client.clone();
        }
        (state.generation, state.config.clone())
    };

    let client = build(&config);
    let mut state = runtime_proxy_state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.generation == generation {
        state.clients.insert(cache_key, client.clone());
    }
    client
}

pub fn set_runtime_proxy_config(config: ProxyConfig) {
    let mut state = runtime_proxy_state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.config = config;
    state.generation = state.generation.wrapping_add(1);
    state.clients.clear();
}

pub fn runtime_proxy_config() -> ProxyConfig {
    runtime_proxy_state()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .config
        .clone()
}

/// Reapply the persisted canonical `[proxy]` config at process startup.
///
/// The model-visible `proxy_config` tool no longer registers, so the
/// persisted config file is the only way proxy state changes; the daemon
/// must therefore make the loaded file effective on its own:
///
/// - the runtime proxy global (read by every proxy-aware client builder)
///   is seeded unconditionally, whatever the scope, and
/// - process proxy environment variables are broadcast only when the
///   config is enabled with `scope = "environment"`, matching the
///   condition the retired `apply_env` action enforced.
///
/// A config that fails `validate()` is applied disabled (mirroring the
/// original config-load behavior): the global gets the explicit default
/// and no environment variable is written. Callers must invoke this once
/// per process boot, before the multi-threaded async runtime (and any
/// task that reads proxy environment variables) starts — process-env
/// mutation is only sound there. A daemon reload refreshes only the
/// runtime global (`set_runtime_proxy_config`), keeping live-env
/// application restart-only.
pub fn apply_persisted_proxy_on_boot(proxy: &ProxyConfig) {
    let mut effective = proxy.clone();
    if let Err(error) = effective.validate() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": error.to_string()})),
            "Invalid persisted proxy configuration ignored at startup; applying disabled proxy"
        );
        effective.enabled = false;
    }
    set_runtime_proxy_config(effective.clone());
    if effective.enabled && effective.scope == ProxyScope::Environment {
        apply_to_process_env(&effective);
    }
}

pub fn apply_runtime_proxy_to_builder(
    builder: reqwest::ClientBuilder,
    service_key: &str,
) -> reqwest::ClientBuilder {
    apply_proxy_to_builder(&runtime_proxy_config(), builder, service_key)
}

pub fn build_runtime_proxy_client(service_key: &str) -> reqwest::Client {
    let cache_key = runtime_proxy_cache_key(service_key, None, None);
    build_cached_proxy_client(cache_key, |config| {
        let builder = apply_proxy_to_builder(config, reqwest::Client::builder(), service_key);
        builder.build().unwrap_or_else(|error| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(
                        ::serde_json::json!({"service_key": service_key, "error": error.to_string()})
                    ),
                "Failed to build proxied client: "
            );
            reqwest::Client::new()
        })
    })
}

pub fn build_runtime_proxy_client_with_timeouts(
    service_key: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    let cache_key =
        runtime_proxy_cache_key(service_key, Some(timeout_secs), Some(connect_timeout_secs));
    build_cached_proxy_client(cache_key, |config| {
        let builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs));
        let builder = apply_proxy_to_builder(config, builder, service_key);
        builder.build().unwrap_or_else(|error| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(
                        ::serde_json::json!({"service_key": service_key, "error": error.to_string()})
                    ),
                "Failed to build proxied timeout client: "
            );
            reqwest::Client::new()
        })
    })
}

/// Build an HTTP client for a channel, using an explicit per-channel proxy URL
/// when configured. Falls back to the global runtime proxy when `proxy_url` is
/// `None` or empty.
pub fn build_channel_proxy_client(service_key: &str, proxy_url: Option<&str>) -> reqwest::Client {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => build_explicit_proxy_client(service_key, &url, None, None),
        None => build_runtime_proxy_client(service_key),
    }
}

/// Build an HTTP client for a channel with custom timeouts, using an explicit
/// per-channel proxy URL when configured. Falls back to the global runtime
/// proxy when `proxy_url` is `None` or empty.
pub fn build_channel_proxy_client_with_timeouts(
    service_key: &str,
    proxy_url: Option<&str>,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => build_explicit_proxy_client(
            service_key,
            &url,
            Some(timeout_secs),
            Some(connect_timeout_secs),
        ),
        None => build_runtime_proxy_client_with_timeouts(
            service_key,
            timeout_secs,
            connect_timeout_secs,
        ),
    }
}

/// Apply an explicit proxy URL to a `reqwest::ClientBuilder`, returning the
/// modified builder. Used by channels that specify a per-channel `proxy_url`.
pub fn apply_channel_proxy_to_builder(
    builder: reqwest::ClientBuilder,
    service_key: &str,
    proxy_url: Option<&str>,
) -> reqwest::ClientBuilder {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => apply_explicit_proxy_to_builder(builder, service_key, &url),
        None => apply_runtime_proxy_to_builder(builder, service_key),
    }
}

/// Build a client with a single explicit proxy URL (http+https via `Proxy::all`).
fn build_explicit_proxy_client(
    service_key: &str,
    proxy_url: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> reqwest::Client {
    let cache_key = format!(
        "explicit|{}|{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        proxy_url,
        timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    build_cached_proxy_client(cache_key, |_| {
        let mut builder = reqwest::Client::builder();
        if let Some(t) = timeout_secs {
            builder = builder.timeout(std::time::Duration::from_secs(t));
        }
        if let Some(ct) = connect_timeout_secs {
            builder = builder.connect_timeout(std::time::Duration::from_secs(ct));
        }
        builder = apply_explicit_proxy_to_builder(builder, service_key, proxy_url);
        builder.build().unwrap_or_else(|error| {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"service_key": service_key, "proxy_url": proxy_url, "error": error.to_string()})), "Failed to build channel proxy client: ");
            reqwest::Client::new()
        })
    })
}

/// Apply a single explicit proxy URL to a builder via `Proxy::all`.
fn apply_explicit_proxy_to_builder(
    mut builder: reqwest::ClientBuilder,
    service_key: &str,
    proxy_url: &str,
) -> reqwest::ClientBuilder {
    match reqwest::Proxy::all(proxy_url) {
        Ok(proxy) => {
            builder = builder.proxy(proxy);
        }
        Err(error) => {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": proxy_url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid channel proxy_url: ");
        }
    }
    builder
}

/// Read a response without retaining more than the configured byte budget.
/// Adapted from upstream a6bd19174bb028c7f3236ea9a389a2305e40c880.
/// `None` preserves the explicit unlimited setting. Known oversize bodies fail
/// before polling; unknown-length bodies stop at the cap without waiting for EOF.
pub async fn read_bounded_body(
    mut response: reqwest::Response,
    limit: Option<usize>,
) -> Result<(Vec<u8>, bool), Box<dyn std::error::Error + Send + Sync>> {
    let content_length = response.content_length();
    if let Some(cap) = limit
        && response
            .content_length()
            .is_some_and(|length| length > cap as u64)
    {
        return Err(std::io::Error::other("Response Content-Length exceeds size limit").into());
    }
    let mut bytes = Vec::new();
    loop {
        if limit.is_some_and(|cap| bytes.len() >= cap) {
            let complete = content_length.is_some_and(|length| length == bytes.len() as u64);
            return Ok((bytes, !complete));
        }
        let Some(chunk) = response.chunk().await? else {
            return Ok((bytes, false));
        };
        let take = limit.map_or(chunk.len(), |cap| chunk.len().min(cap - bytes.len()));
        bytes.extend_from_slice(&chunk[..take]);
    }
}

#[cfg(test)]
mod tests;
