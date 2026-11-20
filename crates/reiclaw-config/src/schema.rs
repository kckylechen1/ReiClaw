//! Current configuration declarations and typed accessors.

use crate::load::default_path_under_config_dir;
pub use crate::load::{
    RuntimeConfigKind, classify_runtime_config_kind, ensure_bootstrap_files,
    read_persisted_proxy_for_boot, resolve_config_dir_for_data, resolve_runtime_dirs,
};

pub(crate) use crate::save::lookup_prop_value_in_table;
#[cfg(test)]
use crate::save::sync_directory;

use crate::autonomy::AutonomyLevel;
pub use crate::bridge_access::{GatewayDeviceBridgeConfig, GatewayOwnerBridgeConfig};
pub use crate::operator_access::GatewayOperatorConfig;
pub use crate::state::{
    EnvOverriddenPaths, ExternalRefSnapshots, PreOverrideSnapshots, SharedStringMap,
    SharedStringSet, TrackedPropIdentity,
};
use crate::traits::{ChannelConfig, HasPropKind, PropKind};
use anyhow::Result;
use directories::UserDirs;
use reiclaw_macros::Configurable;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[cfg(test)]
use crate::validate::{MCP_MAX_TOOL_TIMEOUT_SECS, validate_mcp_config};
use crate::validate::{SUPPORTED_PROXY_SERVICE_KEYS, SUPPORTED_PROXY_SERVICE_SELECTORS};
pub use crate::validate::{validate_memory_semantics, validate_temperature};

// Keep the existing event source when behavior moves to sibling modules.
pub(crate) const CONFIG_SCHEMA_LOG_TARGET: &str = module_path!();

// ── Top-level config ──────────────────────────────────────────────

/// Top-level ReiClaw configuration, loaded from `config.toml`.
///
/// Resolution order: `REI_CONFIG_DIR` env -> `REI_WORKSPACE` env -> `~/.rei/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct Config {
    /// Shared instance data directory (databases, hygiene state, cost
    /// records, daemon state files). Computed from `REI_CONFIG_DIR`
    /// / `REI_DATA_DIR` / `REI_WORKSPACE` at
    /// load time, not serialized. Per-agent identity + markdown lives
    /// at `agent_workspace_dir(&alias)`, not here.
    #[serde(skip)]
    pub data_dir: PathBuf,
    /// Path to config.toml - computed from home, not serialized
    #[serde(skip)]
    pub config_path: PathBuf,
    /// Dotted prop-paths overridden by `REI_*` env vars at load time.
    /// Populated by `apply_env_overrides`; consulted by `save()` to mask the
    /// env-injected values back to disk-or-default before encryption, and by
    /// `prop_is_env_overridden` for O(1) display-layer lookup (config list,
    /// dashboard, quickstart).
    #[serde(skip)]
    pub env_overridden_paths: EnvOverriddenPaths,
    /// Per-path snapshot of pre-override raw values, captured at apply time
    /// from the post-`decrypt_secrets` in-memory state (so secret entries
    /// hold plaintext, not the display mask). `save()` restores from this
    /// map so env-injected values never reach disk and the operator's
    /// original on-disk credentials survive any save cycle.
    #[serde(skip)]
    pub pre_override_snapshots: PreOverrideSnapshots,
    /// Per-path snapshot of `op://` and `vault:` external secret references captured before
    /// `decrypt_secrets()` resolves them for runtime use. `save()` restores
    /// these references unless the same path was intentionally edited.
    #[serde(skip)]
    pub onepassword_reference_snapshots: ExternalRefSnapshots,
    /// Dotted prop-paths mutated since the last persist; drives the
    /// per-path PATCH applied by `save_dirty()`.
    #[serde(skip)]
    pub dirty_paths: std::collections::HashSet<String>,
    /// Whole-entry `(section_path\0entry_key)` markers recorded by
    /// `create_map_key`, `delete_map_key`, and `rename_map_key` so incremental
    /// and full saves distinguish whole-entry deletions/renames (`fast.api_key`)
    /// from leaf property edits on shorter sibling keys (`fast` -> `api_key`).
    #[serde(skip)]
    pub dirty_map_entries: std::collections::HashSet<String>,
    /// Dotted prop-paths explicitly edited via `set_prop_persistent` or
    /// `set_secret_persistent` since the last persist.
    #[serde(skip)]
    pub explicit_prop_dirty_paths: std::collections::HashSet<String>,
    /// Canonical encoded snapshot key (`encode_prop_snapshot_key`) bound when a
    /// leaf property path was recorded in `dirty_paths`, `explicit_prop_dirty_paths`,
    /// `env_overridden_paths`, or `pre_override_snapshots`, preventing subsequent
    /// map/list key mutations from silently rebinding or shadowing a tracked dotted path.
    #[serde(skip)]
    pub tracked_prop_identities: std::collections::HashMap<String, TrackedPropIdentity>,
    /// Security-critical sections the resilient loader reset to `Default`
    /// because the on-disk block was malformed. Non-empty = posture may be
    /// weaker than intended; exposure gating should refuse to trust the
    /// instance until repaired. Never serialized — a load-time signal.
    #[serde(skip)]
    pub degraded_security: Vec<String>,
    /// Non-security sections the resilient loader reset to `Default`
    /// because the on-disk block was malformed (e.g. a table written where
    /// an array of tables was meant). Never serialized;
    /// a load-time signal the CLI surfaces on stderr so a silently-dropped
    /// section is impossible to miss.
    #[serde(skip)]
    pub degraded_sections: Vec<String>,
    /// Structured deprecation warnings for retired config surfaces,
    /// observed at load time: `REI_gateway__pairing_dashboard__*` env
    /// overrides (ignored by the env-override tombstone) and a
    /// `[gateway.pairing_dashboard]` section left in a config file. This
    /// field is the only place those observations can live — nothing else
    /// in the loaded `Config` remembers them. Replayed by
    /// `collect_warnings()` so the stable-code warning machinery (logs,
    /// gateway API, dashboard) surfaces them like any other warning.
    /// Never serialized — a load-time signal. Sunset: these tombstones are
    /// compatibility shims and will be removed in a later announced
    /// window, after which the paths hard-error or parse-drop silently.
    #[serde(skip)]
    pub retired_surface_warnings: Vec<crate::validation_warnings::ValidationWarning>,
    /// The config file this value was populated from — set by
    /// `load_or_init` (both the existing-file read and the fresh-init
    /// write) and by loaders that re-read a config file before mutating.
    /// `Default`/programmatic construction leaves it `None`, and `save()`
    /// then refuses to overwrite an existing file this value cannot prove
    /// it read, so a near-empty snapshot cannot wipe an operator's config;
    /// `force_save()` is the explicit intentional-overwrite path. Binding
    /// provenance to the path (not a boolean) also refuses a value loaded
    /// from file A that is later repointed at a different existing file B.
    /// Never serialized — a load-time signal.
    #[serde(skip)]
    pub loaded_from: Option<PathBuf>,
    /// Config file schema version.
    #[serde(
        default = "default_schema_version",
        deserialize_with = "deserialize_current_schema_version"
    )]
    pub schema_version: u32,

    /// All configured provider profiles, grouped by category under a
    /// single `[providers]` root. Categories today: `models`, `tts`,
    /// `transcription`. Shape: `[providers.<category>.<type>.<alias>]`,
    /// e.g. `[providers.models.anthropic.default]`,
    /// `[providers.tts.openai.default]`,
    /// `[providers.transcription.groq.default]`.
    #[serde(default)]
    #[nested]
    pub providers: crate::providers::Providers,

    /// Model-routing rules — route `hint:<name>` to specific
    /// model_provider + model combos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[nested]
    #[natural_key = "hint"]
    #[group = "Foundation"]
    pub model_routes: Vec<ModelRouteConfig>,

    /// Embedding-routing rules — route `hint:<name>` to specific
    /// model_provider + model combos for embedding requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[nested]
    #[natural_key = "hint"]
    #[group = "Foundation"]
    pub embedding_routes: Vec<EmbeddingRouteConfig>,

    /// Observability backend configuration (`[observability]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub observability: ObservabilityConfig,

    /// Security subsystem configuration (`[security]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub security: SecurityConfig,

    /// Managed cybersecurity service configuration (`[security_ops]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub security_ops: SecurityOpsConfig,

    /// Runtime adapter configuration (`[runtime]`). Controls native vs Docker execution.
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub runtime: RuntimeConfig,

    /// Reliability settings: retries, backoff, key rotation (`[reliability]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub reliability: ReliabilityConfig,

    /// Scheduler configuration for periodic task execution (`[scheduler]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub scheduler: SchedulerConfig,

    /// Pacing controls for slow/local LLM workloads (`[pacing]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub pacing: PacingConfig,

    /// Skills loading and community repository behavior (`[skills]`).
    #[serde(default)]
    #[nested]
    pub skills: SkillsConfig,

    /// Automatic query classification — maps user messages to model hints.
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub query_classification: QueryClassificationConfig,

    /// Heartbeat configuration for periodic health pings (`[heartbeat]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub heartbeat: HeartbeatConfig,

    /// Declarative cron jobs (`[cron.<alias>]`), alias-keyed.
    ///
    /// Each entry is a named scheduled job synced into the database at
    /// scheduler startup. Subsystem runtime knobs (enable/disable, catch-up,
    /// run-history retention) live on `[scheduler]`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub cron: HashMap<String, CronJobDecl>,

    /// Durable task-list behavior (`[tasks]`, #568): the owner-selected
    /// reminder destination armed occurrences copy at creation.
    #[serde(default, skip_serializing_if = "crate::tasks::TasksConfig::is_unset")]
    #[nested]
    #[group = "Operations"]
    pub tasks: crate::tasks::TasksConfig,

    /// Channel configurations: Telegram and webhook instances (`[channels]`).
    #[serde(default, alias = "channels_config")]
    #[nested]
    pub channels: ChannelsConfig,

    /// Memory backend configuration: memorycore, markdown, embeddings (`[memory]`).
    #[serde(default)]
    #[nested]
    pub memory: MemoryConfig,

    /// Persistent storage model_provider configuration (`[storage]`).
    #[serde(default)]
    #[nested]
    pub storage: StorageConfig,

    /// Gateway server configuration: host, port, pairing, rate limits (`[gateway]`).
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub gateway: GatewayConfig,

    /// Secrets encryption configuration (`[secrets]`).
    #[serde(default)]
    #[nested]
    #[group = "Storage"]
    pub secrets: SecretsConfig,

    /// HTTP request tool configuration (`[http_request]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub http_request: HttpRequestConfig,

    /// Multimodal (image) handling configuration (`[multimodal]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub multimodal: MultimodalConfig,

    /// Web fetch tool configuration (`[web_fetch]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub web_fetch: WebFetchConfig,

    /// Web search tool configuration (`[web_search]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub web_search: WebSearchConfig,

    /// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]`).
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub proxy: ProxyConfig,

    /// Cost tracking and budget enforcement configuration (`[cost]`).
    /// Also hosts the operator-managed rate sheet at
    /// `[cost.rates.<type>.<model>]`.
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub cost: CostConfig,

    /// Aliased agents in this install. Each entry under `[agents.<alias>]`
    /// is one user-facing agent with its own identity, channels, model
    /// provider, risk profile, workspace, and memory scope.
    #[serde(default)]
    #[nested]
    pub agents: HashMap<String, AliasedAgentConfig>,

    /// Named risk/autonomy profiles (`[risk_profiles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub risk_profiles: HashMap<String, RiskProfileConfig>,

    /// Install-wide tool composition selector (`composition`).
    ///
    /// `"minimal"` assembles only the explicit minimal companion membership
    /// (`crate::composition::MINIMAL_TOOL_MEMBERSHIP`); individual tool
    /// `enabled` flags do not widen it back. `"full"` (alias `"legacy"`) is
    /// today's assembly — a transitional opt-in compatibility profile. An
    /// absent field resolves as `"full"` so existing installs never lose
    /// tools on upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[group = "Operations"]
    pub composition: Option<crate::composition::Composition>,

    /// Named runtime/LLM execution profiles (`[runtime_profiles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub runtime_profiles: HashMap<String, RuntimeProfileConfig>,

    /// Named agent cards (`[cards.<alias>]`) — one authored unit carrying an
    /// agent's voice, its tool grants, and the profile supplying the rest of
    /// its authority. A card owns tools; its risk profile owns everything
    /// else.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub cards: HashMap<String, crate::card::AgentCard>,

    /// Named persona dial sets (`[personas.<alias>]`) — how an agent talks.
    ///
    /// Authored, never learned, and unable to widen what an agent may do:
    /// these are delivery settings, in the same family as `reasoning_effort`.
    /// Authority lives on `[risk_profiles.<alias>]`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub personas: HashMap<String, crate::persona::PersonaKnobs>,

    /// Companion-memory store and owner gate (`[companion_memory]`).
    ///
    /// `enable` defaults to false. Store files live under `{data_dir}/companion`
    /// unless `store_dir` is set. `[companion_memory.owner]` declares who may
    /// produce `owner_authored` rows. Unmatched ingress is `shared-operator`
    /// and can never produce `owner_authored`.
    #[serde(
        default,
        skip_serializing_if = "crate::companion::CompanionMemoryConfig::is_unset"
    )]
    #[nested]
    #[group = "Storage"]
    pub companion_memory: crate::companion::CompanionMemoryConfig,

    /// Named skill bundles (`[skill_bundles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub skill_bundles: HashMap<String, SkillBundleConfig>,

    /// Named MCP server bundles (`[mcp_bundles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub mcp_bundles: HashMap<String, McpBundleConfig>,

    /// Named peer groups (`[peer_groups.<name>]`). Each entry binds a
    /// channel, a list of member agents, and optional non-agent
    /// (external) members and a per-group blocklist. Mutual opt-in:
    /// two agents become peers only when both appear in the same
    /// group's `agents`. Empty by default for single-agent installs.
    /// See `crate::multi_agent::PeerGroupConfig`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub peer_groups: HashMap<String, crate::multi_agent::PeerGroupConfig>,

    /// Hooks configuration (lifecycle hooks and built-in hook toggles).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub hooks: HooksConfig,

    /// Voice transcription configuration (Whisper API via Groq).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub transcription: TranscriptionConfig,

    /// Text-to-Speech configuration (`[tts]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub tts: TtsConfig,

    /// External MCP server connections (`[mcp]`).
    #[serde(default, alias = "mcpServers")]
    #[nested]
    pub mcp: McpConfig,

    /// Delegation to external harnesses through Tachi's `tachi_staff` MCP
    /// tool (`[tachi]`). Closed by default; the endpoint must be loopback.
    #[serde(default, skip_serializing_if = "crate::tachi::TachiConfig::is_unset")]
    #[nested]
    #[group = "Integrations"]
    pub tachi: crate::tachi::TachiConfig,

    /// Dynamic node discovery configuration (`[nodes]`).
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub nodes: NodesConfig,

    /// Meta-state for the Quickstart flow (which sections the user has
    /// already walked through). Not user-facing config (`[onboard_state]`).
    #[serde(default)]
    #[nested]
    pub onboard_state: OnboardStateConfig,

    /// Verifiable Intent (VI) credential issuance and constraint checking
    /// (`[verifiable_intent]`). No credential chain verifier exists yet, so the
    /// `vi_verify` tool is not registered for the model and this section does not
    /// currently enable verification of anything.
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub verifiable_intent: VerifiableIntentConfig,

    /// Parse-only tombstone: any explicit `sop` value rejects configuration.
    /// The retired approval and guard settings must never become current authority.
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "crate::current::deserialize_retired_sop_config"
    )]
    #[cfg_attr(feature = "schema-export", schemars(skip))]
    pub sop: HashMap<String, ()>,

    /// Shell tool configuration (`[shell_tool]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub shell_tool: ShellToolConfig,

    /// Escalation routing configuration (`[escalation]`).
    #[serde(default)]
    #[nested]
    pub escalation: EscalationConfig,
}

/// Multi-client workspace isolation configuration.
///
/// When enabled, each client engagement gets an isolated workspace with
/// separate memory, audit, secrets, and tool restrictions.
#[allow(clippy::struct_excessive_bools)]
/// Opaque state the Quickstart flow writes so it can tell, on a
/// re-run, which sections the user has already walked through at least
/// once — which lets it offer "Reconfigure? [y/N]" skip gates instead of
/// forcing users through every field again.
///
/// This is meta-state about the Quickstart flow, not user-facing config.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "onboard_state"]
pub struct OnboardStateConfig {
    /// Section keys the user has completed at least once.
    /// Values are the lowercased Section variant names
    /// (`"workspace"`, `"model_providers"`, …).
    #[serde(default)]
    pub completed_sections: Vec<String>,
    /// `true` once the Quickstart has applied a `BuilderSubmission`
    /// successfully on this install. Web gateway and TUI auto-launch
    /// the Quickstart on startup iff this is `false` **and** no
    /// `agents.*` entries exist (the implicit-completion rule covers
    /// upgrades). The flag is flipped in the same atomic write that
    /// lands the Quickstart submission; re-entering the Quickstart
    /// later to add another agent does not flip it back to `false`.
    #[serde(default)]
    pub quickstart_completed: bool,
}

/// Used by `#[serde(skip_serializing_if)]` on plain `bool` fields to omit
/// them from TOML output when they carry their struct-level default (`false`).
/// Keeps fresh model_provider entries clean — a default-constructed
/// `ModelProviderConfig` for one model_provider family shouldn't write flag fields
/// that only apply to a different family.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One trait per family-endpoint enum. Returns the URL for the chosen
/// variant, such as `https://api.openai.com/v1` for a fixed endpoint.
///
/// Resolution order at runtime is uniform across every model model_provider family:
/// operator's `cfg.uri` first; family endpoint enum's `uri()` second; loud
/// failure when neither is set.
pub trait ModelEndpoint {
    fn uri(&self) -> &'static str;
}

/// Implemented by every `*ModelProviderConfig`. Multi-region families
/// override to return `Some(self.endpoint.uri())`; single-endpoint families
/// inherit the `None` default. Drives `ModelProviders::resolved_endpoint_uri`,
/// which is itself driven by the `for_each_model_provider_slot!` macro — so
/// adding a new family without an impl is a compile error.
pub trait FamilyEndpoint {
    fn endpoint_uri(&self) -> Option<&'static str> {
        None
    }
}

/// Wire protocol flavor for the model_provider client. `responses` routes
/// through OpenAI's Responses API (`POST /v1/responses`);
/// `chat_completions` routes through the legacy `/v1/chat/completions` (or
/// the family's chat-completions-compatible endpoint). New OpenAI provider
/// slots default to `responses`; other families default to chat-completions
/// (or ignore the field) when unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, reiclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    Responses,
    ChatCompletions,
}

impl WireApi {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
        }
    }
}

/// Authentication mode for model model_provider families that support more than one
/// (e.g. Qwen, Minimax can use API key OR OAuth). Families that only support a
/// single auth flow simply omit this field from their config struct.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Standard API key authentication via the `api_key` field.
    #[default]
    ApiKey,
    /// OAuth flow — credential resolution defers to the family runtime impl
    /// (typically reading a vendor-specific token cache or env var).
    OAuth,
}

/// Named model_provider profile definition.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models"]
pub struct ModelProviderConfig {
    /// Secret API token for this model_provider. Grab it from the model_provider's dashboard (OpenAI platform, Anthropic console, OpenRouter keys page, etc.). Stored via the OS keyring when possible; never commit it to config.toml directly.
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Provider implementation to instantiate for this profile. Use this
    /// when a canonical typed slot should run through a compatible
    /// implementation, e.g. `[providers.models.openai.proxy] kind =
    /// "openai-compatible"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Endpoint URI the client hits. Override the family's default endpoint when pointing at a self-hosted gateway (LiteLLM, vLLM, Ollama), a custom proxy, or any non-standard URL. Leave unset to use the family's default URI from its `ModelEndpoint` impl. Set this to the FULL endpoint URL; there is no separate path-suffix field.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Model identifier to send with each request: the ID string from the model_provider's catalog (e.g. `gpt-4o`, `claude-sonnet-4-5`, `llama-3.3-70b`). Must match a model the model_provider actually serves on this account.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Ordered list of other provider aliases to try when every model on this
    /// alias has failed. Each entry is a dotted `<type>.<alias>` reference into
    /// `providers.models` and resolves with its own credentials, endpoint, and
    /// model. A fallback never inherits this alias's key. The walk is
    /// depth-first: this alias's models are exhausted first, then each fallback
    /// alias is descended in turn (applying its own `fallback_models` and
    /// `fallback`). Empty means no provider-level fallback.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<crate::providers::ModelProviderRef>,
    /// Ordered alternate models to try on THIS provider before falling over to
    /// the `fallback` aliases. Same endpoint, key, and headers as the primary
    /// `model`. Only the model identifier changes. Use this when a provider
    /// serves a backup model (e.g. a smaller or older variant) that should be
    /// tried before leaving the provider entirely. Empty means only `model` is
    /// tried.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
    /// Sampling temperature passed to the model. Lower values (0.0–0.3) give
    /// deterministic, near-verbatim output, which fits code, routing, summarization.
    /// Higher values (0.7–1.2) give more varied output, which fits open-ended chat.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// HTTP request timeout in seconds. Bump this for slow local model_providers (Ollama on CPU, big local models) or high-latency networks; leave unset otherwise.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Extra HTTP headers sent with every request. Niche: used for auth bridges, corporate proxies, or custom gateways that demand a tracing header. Most users never touch this; edit `config.toml` directly if you need it.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub extra_headers: HashMap<String, String>,
    /// Wire protocol flavor: `responses` for OpenAI's Responses API (`POST /v1/responses`), `chat_completions` for the legacy chat wire and most OpenAI-compatible gateways. New OpenAI provider slots default to `responses`; other families default to chat-completions (or ignore the field). Only override if you're forcing an unusual combination.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_api: Option<WireApi>,
    /// When true, the client pulls credentials from ReiClaw's stored `openai-codex` auth profile instead of the `api_key` field above. Import an existing Codex CLI login with `rei auth login --model-provider openai-codex --import ~/.codex/auth.json`, or run `rei auth login --model-provider openai-codex`. Turn on only for the OpenAI Codex model_provider; leave off for standard API-key model_providers.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "is_false")]
    #[credential_class = "external_auth_store"]
    pub requires_openai_auth: bool,
    /// Hard cap on response length in tokens. Most models enforce sensible built-in limits already; leave unset unless you specifically need to clip long outputs for cost or latency reasons.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// ModelProvider-specific quirk: fold the system prompt into the first user message instead of sending a separate system role. Only needed for models that reject (or mishandle) a standalone system role, e.g. certain older Mistral variants.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "is_false")]
    pub merge_system_into_user: bool,
    /// Extra JSON parameters to include in API requests.
    /// Merged at the top level of the request body, allowing provider-specific
    /// features (routing, transforms, etc.) without code changes.
    /// Example: `provider_extra = { model_provider = { only = ["Anthropic"] } }`
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_extra: Option<serde_json::Value>,
    /// Per-model pricing for cost tracking, USD per 1M tokens.
    ///
    /// Free-form key/value map. Keys are user-defined model identifiers; an
    /// optional `.input` / `.output` suffix encodes pricing dimension when
    /// the operator wants to split rates. A bare key without a suffix is
    /// used as a flat per-token rate when neither dimension is specified.
    /// Default is empty: cost tracking falls back to "unknown" rates and
    /// only token usage is recorded.
    ///
    /// Example: `pricing = { opus = 15.0, sonnet = 3.0 }`
    /// Or split: `pricing = { "opus.input" = 15.0, "opus.output" = 75.0 }`
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub pricing: HashMap<String, f64>,
    /// Whether stored assistant reasoning should be replayed on outbound
    /// assistant history messages. `Some(false)` strips `reasoning_content`
    /// and `reasoning` before sending. `None` (default) honours the provider's
    /// built-in default (true for most compat providers, false for Groq).
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_assistant_reasoning: Option<bool>,
    /// Pull live token prices for this provider's models from its own
    /// OpenAI-compatible `/models` listing (the gateway is the source of truth
    /// for its prices), filling cost-tracking rates for models the operator
    /// has NOT priced under `[cost.rates]` / `pricing`. Models the gateway does
    /// not price (or providers with no HTTP `/models` listing at all, such as
    /// a provider without HTTP model listing) fall back to the public models.dev
    /// catalog. Configured rates always win; live prices only fill gaps. A
    /// background task refreshes the price snapshot hourly; the cost-recording
    /// path reads the cached snapshot and never blocks on the network. Default
    /// `false`: off means no fetching and behavior identical to a build
    /// without the feature.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "is_false")]
    pub live_pricing: bool,
    /// Override the provider's default for native tool calling.
    /// `None` (default) honors the provider's built-in choice. `Some(true)`
    /// forces native tool calls on, `Some(false)` forces text-fallback.
    /// Currently consulted only by the Groq factory, which defaults to
    /// text-fallback because llama-family Groq models reject native tool
    /// calls with HTTP 400. Setting `native_tools = true` re-enables native
    /// tool calling for Groq models that support it.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tools: Option<bool>,
    /// Enable or disable chain-of-thought thinking for models that support it
    /// (e.g. Qwen3, GLM-4). `true` turns thinking on, `false` turns it off.
    /// `None` (default) lets the model decide. Forwarded as `enable_thinking`
    /// in the request body; mirrors the Ollama provider's `think` field.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    /// Override the provider's vision (image input) capability.
    /// `None` (default) uses the provider family's built-in default. Several
    /// families (llama.cpp, the generic OpenAI-compatible endpoint, etc.)
    /// assume vision-capable because they can serve multimodal models. Set
    /// `vision = false` for a text-only model served by such a family (e.g. a
    /// text LLM behind llama.cpp) so image messages are routed to a configured
    /// `[multimodal] vision_model_provider` instead of being sent to a model
    /// that rejects them. `Some(true)` forces vision on.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Arbitrary key/value pairs forwarded verbatim as a top-level
    /// `chat_template_kwargs` object in the request body of OpenAI-compatible
    /// providers. Consumed by chat-template-aware backends such as vLLM,
    /// SGLang, and llama.cpp to pass model-family template variables that
    /// control behaviour not exposed by other fields. Must be a JSON object
    /// (TOML inline table); non-object values are ignored with a warning.
    /// Example (Qwen3 thinking suppression):
    ///   `chat_template_kwargs = { enable_thinking = false }`
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
    /// Send the runtime `reasoning_effort` to every model on this
    /// OpenAI-compatible provider. By default it is sent only to OpenAI
    /// reasoning models (o1/o3/o4/gpt-5, gpt-*codex*), because some backends
    /// reject unknown request fields with HTTP 400. Turn this on only for a
    /// backend known to accept `reasoning_effort`, such as GLM, Kimi,
    /// DeepSeek, or Qwen reasoners behind an OpenAI-compatible gateway.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning_effort_passthrough: bool,
    /// Context window size (max input tokens) for this model.
    /// Auto-populated on setup from provider's /models endpoint if available.
    /// Override manually for custom endpoints or when auto-detection fails.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Path to a PEM-encoded CA certificate for TLS connections to this provider.
    /// Must be an absolute path; shell expansion (e.g. `~`) is not performed.
    /// Leave unset to use the system's default trust store.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_cert_path: Option<String>,
}

// ── Per-family model model_provider configs ────────────────────────────
//
// Each family carries its own typed config (composing `ModelProviderConfig`
// via `#[serde(flatten)]`) plus a per-family `*Endpoint` enum that names the
// known endpoints and resolves them via the `ModelEndpoint` trait. Families
// that support multiple auth flows additionally carry an `auth_mode` field.
//
// Pattern reference for adding a new family:
// - Single-endpoint family with no extras: see `AnthropicModelProviderConfig`
// - Family with extras: see `OpenAIModelProviderConfig`
// - Multi-region family with a required `endpoint` field: see `MoonshotModelProviderConfig`
//
// The `ModelProviders` container in `crates/reiclaw-config/src/model_providers.rs`
// holds a typed slot per family; the runtime impls in zeroclaw-providers
// consume the typed configs directly.

// ── OpenAI ──

/// OpenAI canonical endpoint. Single variant — OpenAI publishes one base URL.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenAIEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for OpenAIEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.openai.com/v1",
        }
    }
}

/// OpenAI model model_provider config. The OpenAI-family extras (`wire_api`,
/// `requires_openai_auth`) live on the shared `ModelProviderConfig` base
/// because they're consumed by validation and runtime helpers that operate
/// on the base struct without family awareness; this wrapper is a thin
/// typed slot, no extra fields.
///
/// New OpenAI provider entries **persisted via `create_map_key` / `ensure`**
/// (quickstart, gateway/config UI, programmatic slot creation) default to
/// `wire_api = "responses"` because OpenAI has moved recent GPT models onto
/// `POST /v1/responses` as the primary wire. OpenAI-compatible families
/// (`custom`, `llamacpp`, branded vendors, …) keep the shared
/// `ModelProviderConfig` default of unset / chat-completions.
///
/// This default governs **persisted slot creation only**. Two paths keep the
/// legacy chat-completions wire for backward compatibility:
/// - Existing persisted configs that omit `wire_api` deserialize as `None`, and
///   the factory falls back to chat-completions.
/// - Implicit dispatch with no config entry at all — a bare
///   `model_provider = "openai"` reference or a dotted ref to a nonexistent
///   alias — is built from a chat-anchored fallback config (see
///   `openai_missing_entry_fallback_config` in `zeroclaw-providers`), not this
///   `Default`, and stays on the chat wire so existing bare-ref installs don't
///   flip wire + tool-calling mode on upgrade.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.openai"]
pub struct OpenAIModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

impl Default for OpenAIModelProviderConfig {
    fn default() -> Self {
        Self {
            base: ModelProviderConfig {
                // OpenAI's current default wire for new provider slots.
                wire_api: Some(WireApi::Responses),
                ..ModelProviderConfig::default()
            },
        }
    }
}

// ── Anthropic ──

/// Anthropic canonical endpoint.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AnthropicEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for AnthropicEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.anthropic.com",
        }
    }
}

/// Anthropic model model_provider config. No family-specific extras yet — typed
/// slot reserved for future Anthropic-only knobs (cache_control, beta
/// headers) so they land cleanly without another schema rework.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.anthropic"]
pub struct AnthropicModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Qwen (multi-region + auth_mode exemplar) ──

/// Qwen endpoint variants. Operators pick the region matching their account.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum QwenEndpoint {
    /// Mainland China (DashScope).
    Cn,
    /// International (alicloud international).
    #[default]
    Intl,
    /// United States (DashScope US).
    Us,
    /// Code-specialist endpoint.
    Code,
}

impl ModelEndpoint for QwenEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Self::Intl => "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            Self::Us => "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
            Self::Code => {
                "https://dashscope.aliyuncs.com/api/v1/services/aigc/text-generation/generation"
            }
        }
    }
}

/// Qwen model model_provider config. Multi-region (`endpoint` required) and
/// supports both API key and OAuth flows (`auth_mode` chooses which).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.qwen"]
pub struct QwenModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    #[serde(default)]
    pub endpoint: QwenEndpoint,
    /// Auth flow. Defaults to `api_key`; set to `oauth` to use the vendor's
    /// OAuth-cache integration instead of the `api_key` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    /// Long-lived Qwen OAuth refresh token. When set, the runtime
    /// exchanges it for a short-lived access token at provider
    /// construction time. Operators relying on the upstream `qwen login`
    /// tool (which writes `~/.qwen/oauth_creds.json`) leave this unset;
    /// the file-cache integration takes over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_refresh_token: Option<String>,
    /// Override of Qwen's published OAuth client_id. Most operators
    /// should leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,
    /// Operator override of the resource URL the refreshed access token
    /// is paired with. When unset, the runtime falls back to the
    /// `endpoint`-derived URL (or the cached `resource_url` when reading
    /// from `~/.qwen/oauth_creds.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_resource_url: Option<String>,
}

impl FamilyEndpoint for QwenModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── Ollama (local-default endpoint) ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OllamaEndpoint {
    #[default]
    LocalDefault,
}

impl ModelEndpoint for OllamaEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:11434",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.ollama"]
pub struct OllamaModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Groq ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GroqEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for GroqEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.groq.com/openai/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.groq"]
pub struct GroqModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── OVH ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OvhEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for OvhEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.ovh"]
pub struct OvhModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Minimax (multi-region + auth_mode) ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MinimaxEndpoint {
    Cn,
    #[default]
    Intl,
}
impl ModelEndpoint for MinimaxEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://api.minimaxi.com/v1",
            Self::Intl => "https://api.minimax.io/v1",
        }
    }
}

impl MinimaxEndpoint {
    /// OAuth `/oauth/token` endpoint for this region. Used by
    /// `refresh_minimax_oauth_access_token` to mint short-lived access
    /// tokens from the operator-supplied `oauth_refresh_token`.
    pub fn oauth_token_endpoint(self) -> &'static str {
        match self {
            Self::Cn => "https://api.minimaxi.com/oauth/token",
            Self::Intl => "https://api.minimax.io/oauth/token",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.minimax"]
pub struct MinimaxModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    #[serde(default)]
    pub endpoint: MinimaxEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    /// Long-lived OAuth refresh token issued by MiniMax. When set, the
    /// runtime exchanges it for a short-lived access token at provider
    /// construction time and uses that as the API credential. Operators
    /// who prefer dashboard-generated long-lived API keys can leave this
    /// unset and populate `api_key` directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_refresh_token: Option<String>,
    /// Override of MiniMax's published OAuth client_id. Most operators
    /// should leave this unset; the runtime defaults to the
    /// vendor-published client_id (same one MiniMax's own portal uses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,
}

impl FamilyEndpoint for MinimaxModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── Gemini (OAuth-capable) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GeminiEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for GeminiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://generativelanguage.googleapis.com/v1beta",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.gemini"]
pub struct GeminiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Auth flow. Defaults to `api_key`; `oauth` uses GeminiModelProvider's
    /// OAuth-cache integration instead of the `api_key` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    /// Google OAuth app `client_id`, used when this alias drives ReiClaw's
    /// own browser/device-code login flow (`rei auth login
    /// --model-provider gemini --profile <alias>`). Operators relying on
    /// the upstream `gemini login` tool don't need this; that tool writes
    /// its own client_id / client_secret into `~/.gemini/oauth_creds.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_client_id: Option<String>,
    /// Google OAuth app `client_secret`. Set alongside `oauth_client_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_client_secret: Option<String>,
    /// Pin a specific GCP project ID for the OAuth `loadCodeAssist`
    /// discovery call. When unset, the discovery probes for an
    /// already-onboarded project on the credential's account. Replaces
    /// `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_PROJECT_ID` env vars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_project: Option<String>,
}

// ── LMStudio (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LmstudioEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for LmstudioEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:1234/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.lmstudio"]
pub struct LmstudioModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── llama.cpp (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlamacppEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for LlamacppEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:8080/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.llamacpp"]
pub struct LlamacppModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Osaurus (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OsaurusEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for OsaurusEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:1337/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.osaurus"]
pub struct OsaurusModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Custom (user-supplied URL, no canonical default) ──

/// Custom catch-all for operator-defined endpoints. The endpoint variant has
/// no canonical URL — operators must always set `base.uri`. The trait return
/// is a sentinel string; the runtime constructor must verify `base.uri` is
/// set for `custom` entries and fail with a clear error if not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CustomEndpoint {
    #[default]
    OperatorSupplied,
}
impl ModelEndpoint for CustomEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::OperatorSupplied => "operator-supplied:set-cfg-uri",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.custom"]
pub struct CustomModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── FamilyEndpoint default impls (single-endpoint families) ─────
//
// Multi-endpoint families (Qwen, Minimax) define
// their own `impl FamilyEndpoint` next to the struct. Every other family
// gets the `None` default via this list. The list is exhaustive: a new
// family with no impl here AND no manual impl elsewhere will fail to
// compile against `ModelProviders::resolved_endpoint_uri`, which expands
// `endpoint_uri()` per slot through `for_each_model_provider_slot!`.

macro_rules! impl_default_family_endpoint {
    ($($t:ty),+ $(,)?) => {
        $( impl FamilyEndpoint for $t {} )+
    };
}

impl_default_family_endpoint! {
    OpenAIModelProviderConfig,
    AnthropicModelProviderConfig,
    OllamaModelProviderConfig,
    GroqModelProviderConfig,
    OvhModelProviderConfig,
    GeminiModelProviderConfig,
    LmstudioModelProviderConfig,
    LlamacppModelProviderConfig,
    OsaurusModelProviderConfig,
    CustomModelProviderConfig,
}

// ── Aliased Agents ───────────────────────────────────────────────

/// Runtime tunables resolved from the agent's runtime profile. Populated
/// by `Config::resolved_agent_config`; never deserialized from the agent
/// table. The runtime profile is the sole config surface for these.
#[derive(Debug, Clone)]
pub struct ResolvedRuntime {
    pub compact_context: bool,
    pub max_tool_iterations: usize,
    pub max_history_messages: usize,
    /// Token budget for preemptive context/history trimming (from runtime profile).
    /// NOT the provider `max_tokens` output limit.
    pub max_context_tokens: usize,
    /// Model's context window (max input tokens) — from provider config.
    pub model_context_window: usize,
    pub parallel_tools: bool,
    pub tool_dispatcher: String,
    pub strict_tool_parsing: bool,
    pub tool_call_dedup_exempt: Vec<String>,
    pub tool_filter_groups: Vec<ToolFilterGroup>,
    pub max_system_prompt_chars: usize,
    pub thinking: crate::scattered_types::ThinkingConfig,
    pub history_pruning: crate::scattered_types::HistoryPrunerConfig,
    pub eval: crate::scattered_types::EvalConfig,
    pub auto_classify: Option<crate::scattered_types::AutoClassifyConfig>,
    pub context_compression: crate::scattered_types::ContextCompressionConfig,
    pub max_tool_result_chars: usize,
    pub keep_tool_context_turns: usize,
    pub tool_receipts: ToolReceiptsConfig,
    pub prompt_injection_mode: SkillsPromptInjectionMode,
}

impl ResolvedRuntime {
    /// Effective token budget for preemptive whole-turn history trimming.
    /// When `history_pruning.enabled` is set, an explicit `max_tokens` floor
    /// trims earlier than the hard context ceiling; otherwise the ceiling is
    /// the only trigger. Reuses the existing `history_pruning.*` idents.
    pub fn effective_context_budget(&self) -> usize {
        if self.history_pruning.enabled && self.history_pruning.max_tokens > 0 {
            self.max_context_tokens.min(self.history_pruning.max_tokens)
        } else {
            self.max_context_tokens
        }
    }
}

impl Default for ResolvedRuntime {
    fn default() -> Self {
        Self {
            compact_context: true,
            max_tool_iterations: 10,
            max_history_messages: 50,
            max_context_tokens: 32_000,
            model_context_window: 32_000,
            parallel_tools: false,
            tool_dispatcher: default_agent_tool_dispatcher(),
            strict_tool_parsing: false,
            tool_call_dedup_exempt: Vec::new(),
            tool_filter_groups: Vec::new(),
            max_system_prompt_chars: default_max_system_prompt_chars(),
            thinking: crate::scattered_types::ThinkingConfig::default(),
            history_pruning: crate::scattered_types::HistoryPrunerConfig::default(),
            eval: crate::scattered_types::EvalConfig::default(),
            auto_classify: None,
            context_compression: crate::scattered_types::ContextCompressionConfig::default(),
            max_tool_result_chars: default_max_tool_result_chars(),
            keep_tool_context_turns: default_keep_tool_context_turns(),
            tool_receipts: ToolReceiptsConfig::default(),
            prompt_injection_mode: SkillsPromptInjectionMode::default(),
        }
    }
}

/// Configuration for an aliased agent. Each `[agents.<alias>]` TOML
/// block deserializes into one of these.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "delegate_agent"]
pub struct AliasedAgentConfig {
    /// Whether this agent is active. Set false to disable without removing the definition.
    #[tab(General)]
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel aliases this agent handles (e.g. `["telegram.<alias>", "webhook.<alias>"]`).
    /// Each entry is a `ChannelRef` resolving through `[channels.<type>.<alias>]`;
    /// `Config::validate()` fails loud on dangling references.
    #[tab(Channels)]
    #[serde(default)]
    pub channels: Vec<crate::providers::ChannelRef>,
    /// Dotted model-provider alias (e.g. `"anthropic.<alias>"`).
    /// Resolves through `model_providers.<type>.<alias>` at runtime;
    /// `Config::validate()` fails loud on dangling references.
    #[tab(Providers)]
    #[serde(default)]
    pub model_provider: crate::providers::ModelProviderRef,
    /// Risk profile alias (e.g. `"default"`). Resolves delegation guardrails at runtime.
    #[tab(General)]
    #[serde(default)]
    pub risk_profile: crate::providers::RiskProfileRef,
    /// Which `[personas.<alias>]` dial set shapes this agent's voice. Absent
    /// leaves every dial at `medium`, which renders no persona text at all.
    #[tab(General)]
    #[serde(default)]
    pub persona: crate::providers::PersonaRef,
    /// Which `[cards.<alias>]` defines this agent. A card supersedes the
    /// separate `persona` and `risk_profile` pointers: setting both a card and
    /// either of those is refused at validation rather than resolved by a
    /// precedence rule nobody would remember.
    #[tab(General)]
    #[serde(default)]
    pub card: crate::providers::CardRef,
    /// Runtime profile alias (e.g. `"default"`). Resolves agentic/iteration settings.
    #[tab(General)]
    #[serde(default)]
    pub runtime_profile: crate::providers::RuntimeProfileRef,
    /// Skill bundle aliases. Each entry resolves to
    /// `skill_bundles[key].directory` at runtime; the agent loads every
    /// listed bundle.
    #[tab(Bundles)]
    #[serde(default)]
    pub skill_bundles: Vec<String>,
    /// MCP bundle aliases. Each entry references `mcp_bundles[key]`, a named
    /// group of MCP servers. Secure by default: an agent is granted only the
    /// servers named by its bundles. An agent with no `mcp_bundles` receives
    /// no MCP servers (omission is not a grant). See
    /// `Config::mcp_servers_for_agent`.
    #[tab(Bundles)]
    #[serde(default)]
    pub mcp_bundles: Vec<String>,
    /// Initialize this agent's `mcp_bundles` tools when it serves an ACP
    /// (`session/new`) session.
    ///
    /// Off by default: MCP servers are external processes/services that can
    /// block startup while they connect, and ACP `session/new` is expected to
    /// return promptly. Enable it when this agent must call its `mcp_bundles`
    /// tools over ACP; `session/new` then pays the one-time MCP connection cost
    /// (bounded and non-fatal per server). Set per agent so each ACP profile
    /// opts in independently. The standalone ACP bridge selects its agent
    /// explicitly; there is no kernel-wide ACP default agent.
    #[tab(Bundles)]
    #[serde(default)]
    pub acp_enable_mcp: bool,
    /// Cron job aliases. Each entry references `cron[key]`, a declarative
    /// scheduled job invoked by the scheduler on its configured trigger.
    /// When the cron fires, this agent is the actor that executes the job.
    #[tab(Cron)]
    #[serde(default)]
    pub cron_jobs: Vec<String>,
    /// TTS provider as a dotted alias reference (`<type>.<alias>`,
    /// e.g. `"openai.<alias>"`). Resolves through `tts_providers.<type>.<alias>`.
    /// Empty = no TTS for this agent (there is no global default-provider concept;
    /// every agent that wants TTS sets its own `tts_provider`).
    #[tab(Providers)]
    #[serde(default)]
    pub tts_provider: crate::providers::TtsProviderRef,
    /// Transcription / STT provider as a dotted alias reference
    /// (`<type>.<alias>`, e.g. `"groq.<alias>"`). Resolves through
    /// `transcription_providers.<type>.<alias>`. Empty = agent has no
    /// transcription preference; channels that ingest voice still need a
    /// resolved provider (there is no global default), so an inbound voice
    /// flow into an agent with empty `transcription_provider` errors loudly
    /// at the channel boundary.
    #[tab(Providers)]
    #[serde(default)]
    pub transcription_provider: crate::providers::TranscriptionProviderRef,

    /// Optional override for the per-message LLM reply-intent classifier
    /// (`classify_channel_reply_intent` in zeroclaw-channels). When non-empty,
    /// the channel orchestrator routes the "should this message be replied to?"
    /// classification call to `[providers.models.<type>.<alias>]` referenced
    /// here, instead of reusing the main agent's `model_provider`.
    ///
    /// Source of truth for api_key / uri / model / temperature etc. is the
    /// referenced `[providers.models.<type>.<alias>]` entry. This field is
    /// a reference only (NEVER a copy), per AGENTS.md SINGLE SOURCE OF TRUTH.
    ///
    /// Empty (`Default`) = inherit the main agent's resolved provider+model
    /// (preserves pre-PR behavior; backward compatible).
    ///
    /// Use case: classification is a cheap REPLY/NO_REPLY decision, doesn't
    /// need a high-end model. Point this at a fast/free small model
    /// (e.g. `kimi-k2.5`, `qwen-turbo`) while `model_provider` stays on the
    /// expensive answering model (e.g. `qwen3.6-plus`).
    ///
    /// Note: TOML table names cannot contain `.`, so alias `kimi-k2.5`
    /// must be written as `[providers.models.custom.kimi-k2-5]`. The
    /// underlying `model = "kimi-k2.5"` string can still contain dots.
    ///
    /// ACP channels (IDE-direct) always reply and skip the classifier
    /// entirely, so this field has no effect on ACP traffic.
    #[tab(Providers)]
    #[serde(default)]
    pub classifier_provider: crate::providers::ModelProviderRef,

    /// Per-agent reply-intent precheck controls. The classifier call reads
    /// this block at message time; model/provider selection stays on
    /// `classifier_provider` so there is only one routing source of truth.
    #[tab(Channels)]
    #[serde(default)]
    #[nested]
    pub precheck: crate::scattered_types::ChannelPrecheckConfig,

    /// Per-agent override for the context-compression summarizer provider, as
    /// a `providers.models.<type>.<alias>` reference. Empty (Default) = inherit
    /// the runtime profile's `context_compression.summary_provider`, else the
    /// agent's own resolved provider+model. Reference only, never a copy;
    /// resolved by [`Config::effective_summary_provider`]. Validated in
    /// `Config::validate()`.
    #[tab(Providers)]
    #[serde(default)]
    pub summary_provider: crate::providers::ModelProviderRef,

    /// Stronger model this agent may consult when a question is hard, as a
    /// typed target: `"model:<type>.<alias>"` names a configured
    /// `[providers.models.<type>.<alias>]` entry. The agent decides when to
    /// consult by calling `reasoning_subagent`, which then runs on this
    /// model instead of the agent's own. Only the objective the agent
    /// writes is sent (no transcript), so the advisor's vendor sees that
    /// text. `"harness:<name>"` is reserved for external-harness advisors
    /// and is refused by validation until they are supported (#381).
    /// Unset (default) = `reasoning_subagent` runs on the agent's own model.
    /// Reference only, never a copy; validated in `Config::validate()`.
    #[tab(Providers)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor: Option<crate::advisor::AdvisorTarget>,

    /// Maximum advisor consultations per turn. Each `reasoning_subagent`
    /// call on an agent with an `advisor` counts once; further calls in the
    /// same turn fail with a tool error. Unset = 2. Must be at least 1.
    #[tab(Providers)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_max_calls_per_turn: Option<u32>,

    // ── Resolved runtime tunables (populated by `resolved_agent_config`
    // from the runtime profile; not config-settable on the agent). ──
    #[serde(skip)]
    pub resolved: ResolvedRuntime,

    /// Per-agent workspace block (`[agents.<alias>.workspace]`).
    /// Holds the agent's filesystem path, cross-agent access allowlist,
    /// filesystem-escape boolean, and cross-agent memory allowlist.
    /// Default is fully jailed (no cross-agent access). See
    /// `crate::multi_agent::AgentWorkspaceConfig`.
    #[tab(Workspace)]
    #[serde(default)]
    #[nested]
    pub workspace: crate::multi_agent::AgentWorkspaceConfig,

    /// Per-agent memory backend selection (`[agents.<alias>.memory]`).
    /// The `backend` field is locked at agent creation and immutable on
    /// subsequent loads. Defaults to `Sqlite`. See
    /// `crate::multi_agent::AgentMemoryConfig`.
    #[tab(Memory)]
    #[serde(default)]
    #[nested]
    pub memory: crate::multi_agent::AgentMemoryConfig,

    /// Per-agent identity format (`[agents.<alias>.identity]`). Each
    /// agent renders its own IDENTITY.md / SOUL.md inside its
    /// per-agent workspace; this block selects the format (OpenClaw or
    /// AIEOS) and optional inline/file source for the agent's identity
    /// document.
    #[tab(Tuning)]
    #[serde(default)]
    #[nested]
    pub identity: IdentityConfig,
}

impl Default for AliasedAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channels: Vec::new(),
            model_provider: crate::providers::ModelProviderRef::default(),
            risk_profile: crate::providers::RiskProfileRef::default(),
            persona: crate::providers::PersonaRef::default(),
            card: crate::providers::CardRef::default(),
            runtime_profile: crate::providers::RuntimeProfileRef::default(),
            skill_bundles: Vec::new(),
            mcp_bundles: Vec::new(),
            acp_enable_mcp: false,
            cron_jobs: Vec::new(),
            tts_provider: crate::providers::TtsProviderRef::default(),
            transcription_provider: crate::providers::TranscriptionProviderRef::default(),
            classifier_provider: crate::providers::ModelProviderRef::default(),
            precheck: crate::scattered_types::ChannelPrecheckConfig::default(),
            summary_provider: crate::providers::ModelProviderRef::default(),
            advisor: None,
            advisor_max_calls_per_turn: None,
            resolved: ResolvedRuntime::default(),
            workspace: crate::multi_agent::AgentWorkspaceConfig::default(),
            memory: crate::multi_agent::AgentMemoryConfig::default(),
            identity: IdentityConfig::default(),
        }
    }
}

/// One `[channels.<type>.<alias>]` block, with the owning agent (if any)
/// resolved via `agents.<agent>.channels`. Returned by
/// `Config::channels_by_alias()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ChannelAliasInfo {
    /// Channel type as the schema emits it (kebab; e.g. `"telegram"`,
    /// `"wecom-ws"`).
    pub channel_type: String,
    /// Per-alias HashMap key (e.g. `"loneliness"`).
    pub alias: String,
    /// The agent whose `channels` list contains `<type>.<alias>`. `None`
    /// when the block is orphaned (config error caught at startup).
    pub owning_agent: Option<String>,
    /// Resolved value of `[channels.<type>.<alias>].enabled` at scan time.
    /// `false` when the field is unset (matches the serde bool default).
    pub enabled: bool,
}

impl Config {
    /// Resolve the configured alias values valid for an [`crate::traits::AliasSource`].
    /// Two-tier sources return dotted `<type>.<alias>` keys; flat sources
    /// return bare alias keys. The single resolver every surface uses for
    /// `PropKind::AliasRef` pickers and validation.
    #[must_use]
    pub fn resolve_alias_source(&self, source: crate::traits::AliasSource) -> Vec<String> {
        use crate::traits::AliasSource;
        let mut out: Vec<String> = match source {
            AliasSource::ModelProviders => self
                .providers
                .models
                .iter_entries()
                .map(|(kind, alias, _)| format!("{kind}.{alias}"))
                .collect(),
            AliasSource::TtsProviders => self
                .providers
                .tts
                .iter_entries()
                .map(|(kind, alias, _)| format!("{kind}.{alias}"))
                .collect(),
            AliasSource::TranscriptionProviders => self
                .providers
                .transcription
                .iter_aliases()
                .map(|(kind, alias)| format!("{kind}.{alias}"))
                .collect(),
            AliasSource::Channels => self
                .channels
                .iter_aliases()
                .filter(|(kind, _, _)| !is_retired_transport(kind))
                .map(|(kind, alias, _)| format!("{kind}.{alias}"))
                .collect(),
            AliasSource::RiskProfiles
            | AliasSource::RuntimeProfiles
            | AliasSource::Personas
            | AliasSource::Cards
            | AliasSource::Agents
            | AliasSource::SkillBundles
            | AliasSource::McpBundles => {
                self.get_map_keys(source.section_path()).unwrap_or_default()
            }
        };
        out.sort();
        out
    }

    /// Return the first concrete `model` string available for use as a
    /// default. Scans every typed slot's entries (iteration order is
    /// the macro slot order) for one with `model` set. Returns `None`
    /// only when no model-provider entry has any model configured at
    /// all.
    #[must_use]
    pub fn resolve_default_model(&self) -> Option<String> {
        self.providers
            .models
            .iter_entries()
            .filter_map(|(_, _, base)| base.model.as_deref().map(str::trim))
            .find(|m| !m.is_empty())
            .map(ToString::to_string)
    }

    /// Resolve the risk profile for an explicit agent alias.
    ///
    /// Each agent's `risk_profile` field names a `[risk_profiles.<alias>]`
    /// entry that gates its actions. There is no "global" risk profile in
    /// every callsite must come through an agent. When the agent has
    /// no profile set or names a missing entry, returns `None` and the
    /// caller decides how to handle it (validation rejects this shape at
    /// load time; the runtime treating `None` as a config error).
    ///
    /// A carded agent (`agents.<alias>.card` set) has an empty
    /// `agent.risk_profile` by construction — validation forbids setting
    /// both — and instead resolves through `cards[card].risk_profile`. That
    /// is the same invariant, only relocated: see `AgentCard::risk_profile`'s
    /// doc, "the card owns tools, the profile owns the rest". This lookup
    /// follows the card so every caller asking "what governs this agent's
    /// autonomy/sandbox/shell allow-list/always_ask" gets an answer whether
    /// or not the agent is carded. Tool grants are a separate concern —
    /// carded agents' `allowed_tools` come from the card, not from whatever
    /// this returns; see `SecurityPolicy::for_agent`.
    #[must_use]
    pub fn risk_profile_for_agent(&self, agent_alias: &str) -> Option<&RiskProfileConfig> {
        let profile_alias = self.resolved_risk_profile_alias(agent_alias)?;
        self.risk_profiles.get(profile_alias)
    }

    /// Resolve the risk-profile *alias* (not the `RiskProfileConfig` itself)
    /// that governs `agent_alias`: a direct `agents.<alias>.risk_profile` if
    /// non-empty, else the card's `risk_profile` when one governs this agent,
    /// else `None`. This is the single resolution [`Self::risk_profile_for_agent`]
    /// performs — extracted so callers that need to *compare* two agents'
    /// governing profile (rather than look one profile up) can do so without
    /// re-deriving the card-follow logic and, in doing so, silently reading
    /// the raw `agent.risk_profile` field instead — the field that is empty
    /// by construction for every carded agent (see that doc comment). Any
    /// site comparing or gating on "this agent's risk profile" should call
    /// this, not `agent.risk_profile` directly.
    ///
    /// Narrower contract than `risk_profile_for_agent`, on purpose: this
    /// returns the alias WITHOUT confirming a `[risk_profiles.<alias>]`
    /// entry exists, so a dangling alias comes back `Some`. That matches
    /// what the raw comparisons it replaced did (they never checked
    /// existence either), and `risk_profile_for_agent` still fails closed
    /// by doing the map lookup on top. A caller that needs "the profile
    /// exists" must use that function, not this one.
    #[must_use]
    fn resolved_risk_profile_alias(&self, agent_alias: &str) -> Option<&str> {
        let agent = self.agents.get(agent_alias)?;
        let profile_alias = agent.risk_profile.trim();
        if !profile_alias.is_empty() {
            return Some(profile_alias);
        }
        let card = agent.card.as_str().trim();
        if card.is_empty() {
            return None;
        }
        let carded_profile = self.cards.get(card)?.risk_profile.as_str().trim();
        if carded_profile.is_empty() {
            return None;
        }
        Some(carded_profile)
    }

    /// True when `agent_alias` names a configured, enabled agent with the
    /// bindings required to dispatch a turn: non-empty `model_provider`, a
    /// risk profile that resolves via [`Self::resolved_risk_profile_alias`]
    /// (so a carded agent's card-supplied profile counts, not just the raw
    /// `agents.<alias>.risk_profile` field, which is empty by construction
    /// for every carded agent), and non-empty `runtime_profile`.
    ///
    /// This lives on `Config`, not `AliasedAgentConfig`, because the
    /// risk-profile leg needs `Config::cards` to follow a card — the same
    /// reason `resolved_risk_profile_alias` itself is a `Config` method
    /// (#21, sixth instance: `AliasedAgentConfig::is_dispatchable` used to
    /// read the raw field directly and reported every valid carded agent as
    /// undispatchable; that method has been removed, this replaces it, and
    /// both call sites in
    /// `zeroclaw-channels/src/orchestrator/acp_server.rs` now call this).
    #[must_use]
    pub fn agent_is_dispatchable(&self, agent_alias: &str) -> bool {
        let Some(agent) = self.agents.get(agent_alias) else {
            return false;
        };
        agent.enabled
            && !agent.model_provider.trim().is_empty()
            && self.resolved_risk_profile_alias(agent_alias).is_some()
            && !agent.runtime_profile.trim().is_empty()
    }

    /// Resolve the persona dial set for an explicit agent alias.
    ///
    /// Mirrors [`Self::risk_profile_for_agent`]'s resolution order exactly:
    /// a direct `agents.<alias>.persona` wins; else follow the card
    /// (`agents.<alias>.card` -> `cards[card].persona`); else `None`. A
    /// carded agent's own `agent.persona` field is empty by construction
    /// (validation forbids setting both a card and a persona), so a carded
    /// agent always resolves through the card once one names a persona.
    /// `None` means "no dials configured" — the governed Soul renderer uses
    /// built-in medium guidance, unlike the risk-profile case where every
    /// enabled agent must resolve to one.
    #[must_use]
    pub fn persona_for_agent(&self, agent_alias: &str) -> Option<&crate::persona::PersonaKnobs> {
        self.persona_selection_for_agent(agent_alias)
            .map(|(knobs, _, _)| knobs)
    }

    /// The canonical persona selection, including its registry alias and
    /// optional card path for derived Tone provenance.
    #[must_use]
    pub fn persona_selection_for_agent(
        &self,
        agent_alias: &str,
    ) -> Option<(&crate::persona::PersonaKnobs, &str, Option<&str>)> {
        let agent = self.agents.get(agent_alias)?;
        let persona = agent.persona.as_str().trim();
        if !persona.is_empty() {
            return self
                .personas
                .get(persona)
                .map(|knobs| (knobs, persona, None));
        }
        let card = agent.card.as_str().trim();
        let persona = self.cards.get(card)?.persona.as_str().trim();
        self.personas
            .get(persona)
            .map(|knobs| (knobs, persona, Some(card)))
    }

    /// Resolve the card that governs an explicit agent alias, if any.
    ///
    /// Mirrors [`Self::risk_profile_for_agent`]'s and
    /// [`Self::persona_for_agent`]'s card-following, but for the card itself
    /// rather than something the card points at: `agents.<alias>.card` names
    /// a `[cards.<alias>]` entry. Returns `None` when the agent has no card
    /// set, or when it names one that does not exist (a dangling reference —
    /// validation rejects this shape at load time; the runtime treats
    /// `None` the same as "uncarded"). This is the single accessor for
    /// "which card governs this agent" — callers that need the card's tool
    /// grants, persona, or risk profile by hand should go through this
    /// rather than re-deriving `agent.card.as_str().trim()` +
    /// `self.cards.get(..)` themselves.
    #[must_use]
    pub fn card_for_agent(&self, agent_alias: &str) -> Option<&crate::card::AgentCard> {
        let agent = self.agents.get(agent_alias)?;
        let card = agent.card.as_str().trim();
        if card.is_empty() {
            return None;
        }
        self.cards.get(card)
    }

    /// Resolve the `[runtime_profiles.<alias>]` entry owned by an agent
    /// (via `agents.<alias>.runtime_profile`). Returns `None` when the
    /// agent has no runtime profile set or names a missing entry. Unlike
    /// `risk_profile_for_agent`, the missing case is not a hard error
    /// because runtime budgets and tunables fall back to global defaults.
    #[must_use]
    pub fn runtime_profile_for_agent(&self, agent_alias: &str) -> Option<&RuntimeProfileConfig> {
        let agent = self.agents.get(agent_alias)?;
        let profile_alias = agent.runtime_profile.trim();
        if profile_alias.is_empty() {
            return None;
        }
        self.runtime_profiles.get(profile_alias)
    }

    /// Effective context-compression summarizer provider for an agent:
    /// agent-level `summary_provider` override → the runtime profile's
    /// `context_compression.summary_provider` → `None` (the caller then reuses
    /// the agent's own provider+model, optionally via the deprecated
    /// `summary_model` swap). Unlike the inert agent-inline tunables below, the
    /// agent-level override IS consulted — it's an explicit per-agent choice,
    /// mirroring `classifier_provider`'s "empty = inherit" semantics.
    #[must_use]
    pub fn effective_summary_provider(
        &self,
        agent_alias: &str,
    ) -> Option<crate::providers::ModelProviderRef> {
        if let Some(a) = self.agents.get(agent_alias)
            && !a.summary_provider.trim().is_empty()
        {
            return Some(a.summary_provider.clone());
        }
        self.runtime_profile_for_agent(agent_alias)
            .map(|p| &p.context_compression.summary_provider)
            .filter(|r| !r.trim().is_empty())
            .cloned()
    }

    // ── Effective per-agent runtime tunables ──────────────────────────
    //
    // Runtime tunables live on `[runtime_profiles.<profile>]`, referenced by an
    // agent via `agents.<alias>.runtime_profile`. A profile value that is
    // explicitly set (non-sentinel, i.e. `> 0` for `max_tool_iterations`) is
    // authoritative; otherwise the global default applies. Agent-inline copies
    // of these tunable keys are inert — superseded by runtime profiles (see the
    // `agent_level_tunable_keys_are_inert` test) — so they are deliberately not
    // consulted here as a fallback.

    #[must_use]
    pub fn effective_max_tool_iterations(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .map(|p| p.max_tool_iterations)
            .filter(|&v| v > 0)
            .unwrap_or(10)
    }

    #[must_use]
    pub fn effective_max_history_messages(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_history_messages)
            .unwrap_or(50)
    }

    /// Resolve the whole-turn history cap used by structured `Agent` sessions.
    ///
    /// An explicit runtime-profile cap remains authoritative. When omitted, the
    /// cap scales with the profile's tool-iteration limit while preserving the
    /// legacy floor of 50 messages.
    #[must_use]
    pub fn effective_structured_max_history_messages(&self, agent_alias: &str) -> usize {
        if let Some(max_history_messages) = self
            .runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_history_messages)
        {
            return max_history_messages;
        }

        // Each tool iteration adds two structural messages; user/final assistant
        // add two more, while the floor preserves the structured default cap of 50.
        self.effective_max_tool_iterations(agent_alias)
            .saturating_mul(2)
            .saturating_add(2)
            .max(50)
    }

    #[must_use]
    pub fn effective_max_context_tokens(&self, agent_alias: &str) -> usize {
        // Token budget for preemptive context/history trimming (runtime profile override).
        // This is NOT the provider max_tokens output limit and NOT the model's context window.
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_context_tokens)
            .unwrap_or(32_000)
    }

    /// The model's context window exactly as configured, or `None` when no
    /// provider profile declares one.
    ///
    /// `None` means *unknown*, not "small". Report it to an operator as unset
    /// rather than resolving it to [`UNCONFIGURED_CONTEXT_WINDOW_FALLBACK`],
    /// which would present a stub as though it were the model's real capacity.
    /// Use [`Config::effective_model_context_window`] when a number is required
    /// for budget arithmetic.
    #[must_use]
    pub fn configured_model_context_window(&self, agent_alias: &str) -> Option<usize> {
        // Provider config (config.toml) is the source of truth for this value.
        self.resolved_model_provider_for_agent(agent_alias)
            .and_then(|(_, _, provider_config)| provider_config.context_window)
    }

    /// Returns the model's context window size (max input tokens).
    /// Source: provider config `context_window` →
    /// [`UNCONFIGURED_CONTEXT_WINDOW_FALLBACK`].
    /// Does NOT check runtime profile (that's for output budget).
    #[must_use]
    pub fn effective_model_context_window(&self, agent_alias: &str) -> usize {
        self.configured_model_context_window(agent_alias)
            .unwrap_or(UNCONFIGURED_CONTEXT_WINDOW_FALLBACK)
    }

    #[must_use]
    pub fn effective_memory_recall_limit(&self, agent_alias: &str) -> usize {
        let raw = self
            .runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.memory_recall_limit)
            .unwrap_or(5);
        if raw == 0 { usize::MAX } else { raw }
    }

    #[must_use]
    pub fn effective_compact_context(&self, agent_alias: &str) -> bool {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.compact_context)
            .unwrap_or(true)
    }

    #[must_use]
    pub fn effective_parallel_tools(&self, agent_alias: &str) -> bool {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.parallel_tools)
            .unwrap_or(false)
    }

    #[must_use]
    pub fn effective_tool_dispatcher(&self, agent_alias: &str) -> String {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.tool_dispatcher.as_ref())
            .filter(|s| !s.trim().is_empty())
            .map_or_else(default_agent_tool_dispatcher, Clone::clone)
    }

    #[must_use]
    pub fn effective_tool_call_dedup_exempt(&self, agent_alias: &str) -> Vec<String> {
        self.runtime_profile_for_agent(agent_alias)
            .map(|p| p.tool_call_dedup_exempt.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn effective_max_system_prompt_chars(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_system_prompt_chars)
            .unwrap_or_else(default_max_system_prompt_chars)
    }

    #[must_use]
    pub fn effective_max_tool_result_chars(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_tool_result_chars)
            .unwrap_or_else(default_max_tool_result_chars)
    }

    #[must_use]
    pub fn effective_keep_tool_context_turns(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.keep_tool_context_turns)
            .unwrap_or_else(default_keep_tool_context_turns)
    }

    /// Return a clone of the named agent's `AliasedAgentConfig` with all
    /// runtime-profile overrides baked in. Use this when an `Agent` (or
    /// any other struct) needs to own a self-contained, already-resolved
    /// view of the agent's runtime knobs without holding a reference to
    /// the full `Config`.
    ///
    /// Returns `None` when `agent_alias` is not present in `agents`.
    ///
    /// Semantics: every field touched here mirrors the matching
    /// `effective_*` helper above. If you add a new runtime_profile knob,
    /// add it both to an `effective_*` helper *and* to this function so
    /// downstream consumers see consistent values regardless of which
    /// surface they read from.
    #[must_use]
    pub fn resolved_agent_config(&self, agent_alias: &str) -> Option<AliasedAgentConfig> {
        let mut out = self.agents.get(agent_alias)?.clone();
        // A card's `grants.mcp_bundles` REPLACES `agents.<alias>.mcp_bundles`,
        // it does not union with it — the same rule the card's tool grants
        // follow (a card is meant to be the complete statement of an agent's
        // reach; a union would mean the card no longer tells you what the
        // agent can reach). Fail closed: a carded agent whose card grants no
        // bundles gets no servers, even if the raw agent field (the pointer
        // the card supersedes) still has entries left over from before it
        // was carded.
        let card = out.card.as_str().trim().to_string();
        if !card.is_empty() {
            out.mcp_bundles = self
                .cards
                .get(card.as_str())
                .map(|c| c.grants.mcp_bundles.clone())
                .unwrap_or_default();
        }
        let mut resolved = ResolvedRuntime {
            max_tool_iterations: self.effective_max_tool_iterations(agent_alias),
            max_history_messages: self.effective_max_history_messages(agent_alias),
            // Token budget for context/history trimming — from runtime profile
            max_context_tokens: self.effective_max_context_tokens(agent_alias),
            // Model's context window (max input tokens) — from provider config
            model_context_window: self.effective_model_context_window(agent_alias),
            compact_context: self.effective_compact_context(agent_alias),
            parallel_tools: self.effective_parallel_tools(agent_alias),
            tool_dispatcher: self.effective_tool_dispatcher(agent_alias),
            tool_call_dedup_exempt: self.effective_tool_call_dedup_exempt(agent_alias),
            max_system_prompt_chars: self.effective_max_system_prompt_chars(agent_alias),
            max_tool_result_chars: self.effective_max_tool_result_chars(agent_alias),
            keep_tool_context_turns: self.effective_keep_tool_context_turns(agent_alias),
            prompt_injection_mode: self.effective_skills_prompt_mode(agent_alias),
            ..ResolvedRuntime::default()
        };
        if let Some(profile) = self.runtime_profile_for_agent(agent_alias) {
            resolved.strict_tool_parsing = profile.strict_tool_parsing;
            resolved.thinking = profile.thinking.clone();
            resolved.history_pruning = profile.history_pruning.clone();
            resolved.eval = profile.eval.clone();
            resolved.auto_classify = profile.auto_classify.clone();
            resolved.context_compression = profile.context_compression.clone();
            resolved.tool_receipts = profile.tool_receipts.clone();
            resolved.tool_filter_groups = profile.tool_filter_groups.clone();
        }
        out.resolved = resolved;
        Some(out)
    }

    /// Resolve the effective skills prompt-injection mode for an agent: the
    /// agent's resolved runtime profile's `prompt_injection_mode` override when
    /// set, otherwise the global `[skills] prompt_injection_mode`. Agents with
    /// no runtime profile (or an unknown alias) fall back to the global value.
    ///
    /// Keyed on the resolved runtime profile — the sanctioned surface for
    /// per-agent runtime tunables — so agent-inline knobs stay inert.
    /// Centralizing the fallback here keeps the system-prompt builder and the
    /// `read_skill` tool-registration gate in lockstep on one effective mode,
    /// so a compact prompt is never paired with a missing `read_skill` tool
    /// (or a full prompt with a spurious one).
    #[must_use]
    pub fn effective_skills_prompt_mode(&self, agent_alias: &str) -> SkillsPromptInjectionMode {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.prompt_injection_mode)
            .unwrap_or(self.skills.prompt_injection_mode)
    }

    /// Resolve an agent's `model_provider` reference (`"<type>.<alias>"`) to
    /// its concrete `ModelProviderConfig` entry. Returns `None` when the
    /// agent doesn't exist, the reference is unparseable, or the
    /// `<type>.<alias>` pair doesn't resolve in `providers.models`.
    ///
    /// This is the lookup the orchestrator uses to build per-agent
    /// model_provider runtime options via explicit `<type>.<alias>`
    /// resolution — there is no concept of a "first" or "default"
    /// provider. The matching split logic lives in
    /// `crates/reiclaw-runtime/src/tools/delegate.rs::resolve_brain` for
    /// the delegation path; this helper exposes the same contract for the
    /// channel-server startup path.
    #[must_use]
    pub fn model_provider_for_agent(&self, agent_alias: &str) -> Option<&ModelProviderConfig> {
        let agent = self.agents.get(agent_alias)?;
        let (type_key, alias_key) = agent.model_provider.split_once('.')?;
        self.providers.models.find(type_key, alias_key)
    }

    /// Resolve `(provider_type, provider_alias, &ModelProviderConfig)` for an
    /// agent. Same lookup as `model_provider_for_agent` but also returns the
    /// `'static` type key that downstream provider factories
    /// (`create_routed_model_provider_with_options`, etc.) need. Returns
    /// `None` when the agent has no `model_provider` set, when the reference
    /// is unparseable, or when the resolved entry has been deleted from
    /// `providers.models`.
    #[must_use]
    pub fn resolved_model_provider_for_agent(
        &self,
        agent_alias: &str,
    ) -> Option<(&'static str, &str, &ModelProviderConfig)> {
        let agent = self.agents.get(agent_alias)?;
        let (type_key, alias_key) = agent.model_provider.split_once('.')?;
        self.providers
            .models
            .iter_entries()
            .find(|(ty, al, _)| *ty == type_key && *al == alias_key)
    }

    /// Reverse-lookup the agent alias that owns a configured channel
    /// (`<type>.<alias>`). Returns the first agent listing the channel in
    /// its `channels` field. `None` when no agent owns the channel —
    /// orphaned channels are a config error the orchestrator surfaces at
    /// startup.
    #[must_use]
    pub fn agent_for_channel(&self, channel_alias: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|(_, agent)| agent.enabled && agent.channels.iter().any(|c| c == channel_alias))
            .map(|(alias, _)| alias.as_str())
    }

    /// Workspace dir a channel's inbound-media handler writes into. Resolves
    /// the channel's owning agent and returns `<install>/agents/<alias>/workspace/`;
    /// falls back to `data_dir` for orphan channels (no owning agent enabled).
    #[must_use]
    pub fn channel_workspace_dir(&self, channel_ref: &str) -> PathBuf {
        self.agent_for_channel(channel_ref)
            .map_or_else(|| self.data_dir.clone(), |a| self.agent_workspace_dir(a))
    }

    /// Every populated `[channels.<type>.<alias>]` block, read from typed maps.
    /// Alias keys remain intact, including dots, and `enabled` is read as a bool.
    ///
    /// Retired transports (`RETIRED_TRANSPORT_KINDS`) are excluded: every
    /// consumer of this walk is a presentation surface (dashboard channel
    /// rows, agent channel pickers, tool channel-ref options), and a
    /// populated retired section must not surface as a usable channel
    /// alias (#584).
    #[must_use]
    pub fn channels_by_alias(&self) -> Vec<ChannelAliasInfo> {
        let mut aliases: Vec<_> = self
            .channels
            .iter_aliases()
            .filter(|(channel_type, _, _)| !is_retired_transport(channel_type))
            .map(|(channel_type, alias, enabled)| {
                let composite = format!("{channel_type}.{alias}");
                let owning_agent = self.agent_for_channel(&composite).map(str::to_string);
                ChannelAliasInfo {
                    channel_type: channel_type.to_string(),
                    alias: alias.to_string(),
                    owning_agent,
                    enabled,
                }
            })
            .collect();
        aliases.sort_by(|a, b| (&a.channel_type, &a.alias).cmp(&(&b.channel_type, &b.alias)));
        aliases
    }

    /// Reverse-lookup the agent alias that owns a declaratively-configured
    /// cron job (`[cron.<alias>]`). Returns the first agent listing the
    /// alias in its `cron_jobs` field. `None` when no agent claims the
    /// job — orphaned cron jobs are skipped at scheduler time with a
    /// warning. Imperative jobs (created at runtime via `cron_add`) have
    /// UUID-shaped ids that won't match any agent's `cron_jobs`; the
    /// scheduler treats those separately (carrying their owning agent
    /// alongside the DB row is a follow-up).
    #[must_use]
    pub fn agent_for_cron_job(&self, cron_alias: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|(_, agent)| agent.enabled && agent.cron_jobs.iter().any(|c| c == cron_alias))
            .map(|(alias, _)| alias.as_str())
    }

    /// Resolve the per-agent workspace directory for `alias`.
    ///
    /// Returns the agent's `[agents.<alias>.workspace.path]` override
    /// when set (operator-explicit, e.g. for putting a workspace on a
    /// different disk), otherwise derives
    /// `<install>/agents/<alias>/workspace/` from the install root
    /// (the directory containing `config.toml`).
    ///
    /// Per-agent workspaces live under
    /// `<install>/agents/<alias>/workspace/` and hold the agent's
    /// markdown memory (MEMORY.md), identity files (IDENTITY.md,
    /// SOUL.md), and any other per-agent plaintext state. Shared
    /// databases (SQLite memory, sessions, cost records) live under
    /// `config.data_dir` instead and partition by agent at the row
    /// level. Per-agent overrides via `[agents.<alias>.workspace.path]`
    /// pin an arbitrary filesystem path (e.g. a different mount).
    #[must_use]
    pub fn agent_workspace_dir(&self, agent_alias: &str) -> std::path::PathBuf {
        if let Some(cfg) = self.agents.get(agent_alias)
            && let Some(custom) = cfg.workspace.path.as_ref()
        {
            return custom.clone();
        }
        self.install_root_dir()
            .join("agents")
            .join(agent_alias)
            .join("workspace")
    }

    /// Effective MCP servers granted to an agent by its `mcp_bundles`.
    ///
    /// Secure by default: omission is not a grant. An agent is reached via
    /// `resolved_agent_config`; an unknown alias or one with no `mcp_bundles`
    /// receives NO MCP servers. See [`Self::mcp_servers_for_bundles`] for how
    /// a non-empty bundle list resolves to servers.
    #[must_use]
    pub fn mcp_servers_for_agent(&self, agent_alias: &str) -> Vec<McpServerConfig> {
        match self.resolved_agent_config(agent_alias) {
            Some(agent) => self.mcp_servers_for_bundles(&agent.mcp_bundles),
            None => Vec::new(),
        }
    }

    /// Resolve a set of `[mcp_bundles.<alias>]` references to the concrete
    /// `[mcp.servers]` entries they grant.
    ///
    /// Secure by default: omission is not a grant.
    /// - An empty `bundle_aliases` grants NO servers (`Vec::new()`).
    /// - The grant is the union of each referenced bundle's `servers`, in
    ///   first-seen order with duplicates dropped.
    /// - Deny wins: a server name listed in ANY referenced bundle's `exclude`
    ///   is removed from the grant, regardless of which bundle included it.
    /// - An unknown bundle alias grants nothing (it is skipped, not an error)
    ///   and a server name with no matching `[mcp.servers]` entry grants
    ///   nothing. Both fail closed: a misconfiguration narrows access, never
    ///   widens it.
    #[must_use]
    pub fn mcp_servers_for_bundles(&self, bundle_aliases: &[String]) -> Vec<McpServerConfig> {
        if bundle_aliases.is_empty() {
            return Vec::new();
        }
        // Deny wins: collect every excluded name across the referenced bundles
        // first, so an include in one bundle cannot defeat an exclude in another.
        let mut excluded: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for alias in bundle_aliases {
            if let Some(bundle) = self.mcp_bundles.get(alias) {
                for name in &bundle.exclude {
                    excluded.insert(name.as_str());
                }
            }
        }
        // Union of granted names in first-seen order, skipping excludes.
        let mut granted: Vec<&str> = Vec::new();
        for alias in bundle_aliases {
            if let Some(bundle) = self.mcp_bundles.get(alias) {
                for name in &bundle.servers {
                    let n = name.as_str();
                    if !excluded.contains(n) && !granted.contains(&n) {
                        granted.push(n);
                    }
                }
            }
        }
        // Resolve names against the configured servers. A name that matches no
        // `[mcp.servers]` entry is not granted.
        granted
            .into_iter()
            .filter_map(|name| self.mcp.servers.iter().find(|s| s.name == name).cloned())
            .collect()
    }

    /// `<install>/shared/` — directory shared across every agent on this
    /// host. Holds skills, skill bundles, and any
    /// other content not scoped to a single agent's workspace. Distinct
    /// from `agent_workspace_dir(alias)` (per-agent state) and
    /// `data_dir` (databases + runtime state).
    #[must_use]
    pub fn shared_workspace_dir(&self) -> std::path::PathBuf {
        self.install_root_dir().join("shared")
    }

    /// Install root: `<install>/` derived from `config_path`'s parent. Used
    /// to compute `<install>/shared/`, `<install>/agents/`, and the
    /// skill-bundle directory defaults. Public so consumers (gateway, CLI,
    /// SkillsService) share the same anchor.
    #[must_use]
    pub fn install_root_dir(&self) -> std::path::PathBuf {
        self.config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Resolve an aliased-agent config by alias. `None` when the alias
    /// isn't configured; callers should treat this as a config error
    /// rather than synthesizing a default.
    #[must_use]
    pub fn agent(&self, agent_alias: &str) -> Option<&AliasedAgentConfig> {
        self.agents.get(agent_alias)
    }

    /// Resolve the runtime-active agent alias the orchestrator binds
    /// channels to. Mirrors the same selection logic as
    /// `start_channels()` in zeroclaw-channels: prefer the migration-
    /// synthesized `"default"` agent, otherwise fall back to the
    /// lexicographically-smallest enabled alias. Returns `None` only
    /// when no enabled agent is configured.
    ///
    /// Used by per-agent infrastructure (TtsManager, TranscriptionManager)
    /// to pick which agent's `tts_provider` / `transcription_provider`
    /// drives the manager's resolved alias. Until the per-channel
    /// dispatch refactor lands, the orchestrator runs in single-agent
    /// mode, so all manager instances share the same resolved agent.
    #[must_use]
    pub fn resolved_runtime_agent_alias(&self) -> Option<&str> {
        self.agents
            .keys()
            .find(|k| k.as_str() == "default")
            .map(String::as_str)
            .or_else(|| {
                self.agents
                    .iter()
                    .filter(|(_, a)| a.enabled)
                    .map(|(alias, _)| alias.as_str())
                    .min()
            })
    }

    /// Resolve the active storage backend for the memory subsystem.
    ///
    /// `MemoryConfig.backend` is a dotted reference (`<backend>.<alias>`) into
    /// `Config.storage.<backend>.<alias>`. Bare backend names are interpreted
    /// as `<backend>.default` for back-compat.
    ///
    /// Returns `ActiveStorage::None` when no backend is configured, when the
    /// backend is `"none"`, or when the dotted alias does not resolve to a
    /// configured entry.
    pub fn resolve_active_storage(&self) -> ActiveStorage<'_> {
        let backend = self.memory.backend.trim();
        if backend.is_empty() || backend.eq_ignore_ascii_case("none") {
            return ActiveStorage::None;
        }
        let (kind, alias) = backend.split_once('.').unwrap_or((backend, "default"));
        match kind {
            "markdown" => self
                .storage
                .markdown
                .get(alias)
                .map(ActiveStorage::Markdown)
                .unwrap_or(ActiveStorage::None),
            _ => ActiveStorage::None,
        }
    }
}

/// Resolved storage backend variant.
///
/// Returned from [`Config::resolve_active_storage`]. Each variant carries a
/// borrow of the typed config from the corresponding `Config.storage` map.
#[derive(Debug, Clone, Copy)]
pub enum ActiveStorage<'a> {
    /// No storage configured (`memory.backend = "none"`, `"memorycore"`, or unresolved alias).
    None,
    /// Markdown directory storage instance.
    Markdown(&'a MarkdownStorageConfig),
}

impl ActiveStorage<'_> {
    /// Backend type name (`"markdown"`, etc.); `"none"` for unconfigured.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            ActiveStorage::None => "none",
            ActiveStorage::Markdown(_) => "markdown",
        }
    }
}

/// Valid temperature range for all paths (config, CLI, env override).
pub const TEMPERATURE_RANGE: std::ops::RangeInclusive<f64> = 0.0..=2.0;

/// Context window assumed when no provider profile declares one.
///
/// Deliberately conservative: it has to be safe for any model, which means it
/// is almost always *wrong* for the model actually in use — a 1M-context model
/// on a profile that omits `context_window` runs against this number. It exists
/// so budget arithmetic has an operand, not because it describes any model.
///
/// Anything reporting to an operator must call
/// [`Config::configured_model_context_window`] and say "not configured" when it
/// returns `None`, rather than echoing this value as the model's real capacity.
pub const UNCONFIGURED_CONTEXT_WINDOW_FALLBACK: usize = 32_000;

/// Partial field probes use current defaults. Whole-file entry points check
/// that the raw input explicitly declared the supported version before serde.
fn default_schema_version() -> u32 {
    crate::current::CURRENT_SCHEMA_VERSION
}

/// Property/JSON edits must not create a live version that persistence would
/// silently restamp. The same guard also applies to typed direct deserialization.
fn deserialize_current_schema_version<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let version = u32::deserialize(deserializer)?;
    if version != crate::current::CURRENT_SCHEMA_VERSION {
        return Err(serde::de::Error::custom(format!(
            "schema_version {version} is unsupported; only schema_version {} is accepted",
            crate::current::CURRENT_SCHEMA_VERSION
        )));
    }
    Ok(version)
}

/// Per-channel reply-pacing accessor. Implemented by every `*Config`
/// struct that participates in outbound pacing so validation and
/// wrapper construction can walk all of them through a single
/// abstraction rather than nine duplicated method calls.
pub trait HasReplyPacing {
    fn reply_min_interval_secs(&self) -> u64;
    fn reply_queue_depth_max(&self) -> u16;
}

macro_rules! impl_reply_pacing {
    ($($ty:ty),+ $(,)?) => {
        $(impl HasReplyPacing for $ty {
            fn reply_min_interval_secs(&self) -> u64 { self.reply_min_interval_secs }
            fn reply_queue_depth_max(&self) -> u16 { self.reply_queue_depth_max }
        })+
    };
}

/// Inclusive upper bound (seconds) for per-channel `reply_min_interval_secs`.
pub const REPLY_MIN_INTERVAL_MAX_SECS: u64 = 3600;

/// Inclusive upper bound for per-channel `reply_queue_depth_max`. The lower
/// bound is `0`, where `0` means "use [`DEFAULT_REPLY_QUEUE_DEPTH`] at the
/// pacing-wrapper construction site." A non-zero value pins the bound
/// explicitly. Validator rejects values above this ceiling.
pub const REPLY_QUEUE_DEPTH_CEILING: u16 = 1024;

/// Fallback queue depth applied at the pacing-wrapper construction site
/// when a channel's `reply_queue_depth_max` is left at `0`. Sized for the
/// AI-pacing use case: a paced channel buffering more than this is a sign
/// the agent is producing replies faster than the floor will ever drain
/// them and the overflow log is the right signal.
pub const DEFAULT_REPLY_QUEUE_DEPTH: u16 = 16;

/// Idle-state LRU cap on the pacing wrapper's per-recipient rows.
/// Bounds growth when a bot legitimately serves many thousands of distinct
/// peers. Eviction only reclaims idle rows (no queued sends, no running
/// worker, no in-flight dispatch), so under a pathological all-active burst
/// the row count can temporarily exceed this target until rows become idle —
/// it is not an unconditional hard bound. Each recipient's queue depth stays
/// bounded regardless. Not exposed in config — promote to a schema field if
/// an operator reports hitting it.
pub const PACING_RECIPIENT_CAP: usize = 1024;

fn normalize_reasoning_effort(value: &str) -> std::result::Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "minimal" | "low" | "medium" | "high" | "xhigh" => Ok(normalized),
        _ => Err(format!(
            "reasoning_effort {value:?} is invalid (expected one of: minimal, low, medium, high, xhigh)"
        )),
    }
}

fn deserialize_reasoning_effort_opt<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<String> = Option::deserialize(deserializer)?;
    value
        .map(|raw| normalize_reasoning_effort(&raw).map_err(serde::de::Error::custom))
        .transpose()
}

fn deserialize_enum_lenient<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(raw).unwrap_or_default())
}

// ── Transcription ────────────────────────────────────────────────

fn default_transcription_api_url() -> String {
    "https://api.groq.com/openai/v1/audio/transcriptions".into()
}

fn default_transcription_model() -> String {
    "whisper-large-v3-turbo".into()
}

fn default_transcription_max_duration_secs() -> u64 {
    120
}

fn default_openai_stt_model() -> String {
    "whisper-1".into()
}

fn default_deepgram_stt_model() -> String {
    "nova-2".into()
}

fn default_google_stt_language_code() -> String {
    "en-US".into()
}

/// Voice transcription configuration with multi-provider support.
///
/// The top-level `api_url`, `model`, and `api_key` fields remain for backward
/// compatibility with existing Groq-based configurations.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription"]
pub struct TranscriptionConfig {
    /// Enable voice transcription for channels that support it.
    #[serde(default)]
    pub enabled: bool,
    /// API key used for transcription requests (Groq transcription provider).
    ///
    /// Use the schema-mirror env grammar (`REI_transcription__api_key=...`)
    /// for runtime env injection. Legacy `GROQ_API_KEY` constructor fallback was
    /// removed in v0.8.0.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Whisper API endpoint URL (Groq transcription provider).
    #[serde(default = "default_transcription_api_url")]
    pub api_url: String,
    /// Whisper model name (Groq transcription provider).
    #[serde(default = "default_transcription_model")]
    pub model: String,
    /// Optional language hint (ISO-639-1, e.g. "en", "ru") for Groq transcription provider.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional initial prompt to bias transcription toward expected vocabulary
    /// (proper nouns, technical terms, etc.). Sent as the `prompt` field in the
    /// Whisper API request.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// Optional global audio size upper bound in bytes, enforced before
    /// dispatching to any transcription provider. Provider-specific caps still
    /// apply.
    #[serde(default)]
    pub max_audio_bytes: Option<usize>,
    /// Maximum voice duration in seconds (messages longer than this are skipped).
    #[serde(default = "default_transcription_max_duration_secs")]
    pub max_duration_secs: u64,
    /// OpenAI Whisper STT model_provider configuration.
    #[serde(default)]
    #[nested]
    pub openai: Option<OpenAiSttConfig>,
    /// Deepgram STT model_provider configuration.
    #[serde(default)]
    #[nested]
    pub deepgram: Option<DeepgramSttConfig>,
    /// AssemblyAI STT model_provider configuration.
    #[serde(default)]
    #[nested]
    pub assemblyai: Option<AssemblyAiSttConfig>,
    /// Google Cloud Speech-to-Text model_provider configuration.
    #[serde(default)]
    #[nested]
    pub google: Option<GoogleSttConfig>,
    /// Local/self-hosted Whisper-compatible STT model_provider.
    #[serde(default)]
    #[nested]
    pub local_whisper: Option<LocalWhisperConfig>,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            api_url: default_transcription_api_url(),
            model: default_transcription_model(),
            language: None,
            initial_prompt: None,
            max_audio_bytes: None,
            max_duration_secs: default_transcription_max_duration_secs(),
            openai: None,
            deepgram: None,
            assemblyai: None,
            google: None,
            local_whisper: None,
        }
    }
}

// ── MCP ─────────────────────────────────────────────────────────

/// Transport type for MCP server connections.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Spawn a local process and communicate over stdin/stdout.
    #[default]
    Stdio,
    /// Connect via HTTP POST.
    Http,
    /// Connect via HTTP + Server-Sent Events.
    Sse,
}

impl McpTransport {
    /// Wire token for this transport, matching the `rename_all = "lowercase"`
    /// serde representation (`stdio`/`http`/`sse`). The exhaustive match makes
    /// a new variant a compile error here rather than a silent miss.
    pub fn wire_name(self) -> &'static str {
        match self {
            McpTransport::Stdio => "stdio",
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
        }
    }

    /// The `McpServerConfig` leaf this transport requires to be non-empty.
    /// Single source of truth shared by `validate_mcp_config` (runtime
    /// enforcement) and the config-form `x-required-by-transport` schema
    /// metadata the Operator Console reads to mark the right field Required
    /// per transport. Changing the relationship here updates both consumers.
    pub fn required_leaf(self) -> &'static str {
        match self {
            McpTransport::Stdio => "command",
            McpTransport::Http | McpTransport::Sse => "url",
        }
    }
}

/// Every transport, enumerated on-demand from the schema rather than a
/// hand-maintained list, so the set is derived from the enum itself and a new
/// variant shows up here automatically (its `required_leaf` arm then forces a
/// decision). schemars emits a fieldless enum as a top-level `enum` array, or
/// as `oneOf` of `const`s once the variants carry doc comments; handle both,
/// matching the existing `enum_variants` helper. The wire tokens honor the
/// serde rename, so they round-trip straight back to the enum.
#[cfg(feature = "schema-export")]
fn mcp_transports() -> Vec<McpTransport> {
    let schema = serde_json::to_value(schemars::schema_for!(McpTransport)).unwrap_or_default();
    let wire_values: Vec<serde_json::Value> =
        if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array) {
            values.clone()
        } else if let Some(variants) = schema.get("oneOf").and_then(serde_json::Value::as_array) {
            variants
                .iter()
                .filter_map(|variant| variant.get("const").cloned())
                .collect()
        } else {
            Vec::new()
        };
    wire_values
        .into_iter()
        .filter_map(|value| serde_json::from_value::<McpTransport>(value).ok())
        .collect()
}

/// Per-transport required-leaf map, projected from [`McpTransport::required_leaf`]
/// onto the schema as the `x-required-by-transport` extension on
/// `McpServerConfig`. The Operator Console config form reads it so the Required
/// badge tracks the selected transport (`stdio` needs `command`, `http`/`sse`
/// need `url`) instead of hardcoding the relationship in the web surface.
#[cfg(feature = "schema-export")]
fn mcp_required_by_transport() -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = mcp_transports()
        .into_iter()
        .map(|transport| {
            (
                transport.wire_name().to_string(),
                serde_json::Value::String(transport.required_leaf().to_string()),
            )
        })
        .collect();
    serde_json::Value::Object(map)
}

/// Configuration for a single external MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema-export",
    schemars(extend("x-required-by-transport" = mcp_required_by_transport()))
)]
#[prefix = "mcp.servers"]
pub struct McpServerConfig {
    /// Display name used as a tool prefix (`<server>__<tool>`). Filled in
    /// from the supplied `map_key` when the entry is created via
    /// `create_map_key("mcp.servers", "<name>")`; `#[serde(default)]` lets
    /// the macro default-construct from `{}` before the name gets injected.
    #[serde(default)]
    pub name: String,
    /// Transport type (default: stdio).
    #[serde(default)]
    pub transport: McpTransport,
    /// URL for HTTP/SSE transports.
    #[serde(default)]
    pub url: Option<String>,
    /// Executable to spawn for stdio transport.
    #[serde(default)]
    pub command: String,
    /// Command arguments for stdio transport.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional environment variables for stdio transport.
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub env: HashMap<String, String>,
    /// Optional HTTP headers for HTTP/SSE transports. Treated as secret:
    /// the values commonly carry Bearer tokens for the upstream MCP server.
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub headers: HashMap<String, String>,
    /// Optional per-call timeout in seconds (hard capped in validation).
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,
    /// Resource URIs to read once at agent startup and inject into the system
    /// prompt as untrusted, server-origin context. Each is read via
    /// `resources/read` on this server; pins on a server that does not advertise
    /// resources, or that the agent's tool policy denies, are skipped with a
    /// warning. Read once per run (not refreshed; no subscriptions).
    #[serde(default)]
    pub pinned_resources: Vec<String>,
}

/// External MCP client configuration (`[mcp]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "mcp"]
pub struct McpConfig {
    /// Enable MCP tool loading.
    #[tab(Settings)]
    #[serde(default = "default_mcp_enabled")]
    pub enabled: bool,
    /// Load MCP tool schemas on-demand via `tool_search` instead of eagerly
    /// including them in the LLM context window. When enabled, only tool names
    /// are listed in the system prompt; the LLM must call `tool_search` to fetch
    /// full schemas before invoking a deferred tool.
    #[tab(Settings)]
    #[serde(default = "default_deferred_loading")]
    pub deferred_loading: bool,
    /// Configured MCP servers. The `#[nested]` annotation makes the macro
    /// expose this as a List section in `map_key_sections()`, so the
    /// dashboard's `+ Add MCP server` affordance and the `POST
    /// /api/config/map-key?path=mcp.servers&key=<name>` endpoint pick it
    /// up automatically (no hand-table on the gateway side).
    ///
    /// `#[natural_key = "name"]` opts the Vec into per-element property
    /// routing (see `route_vec_path` and the `Configurable` derive's
    /// `#[natural_key]` arm). With it, `set_prop("mcp.servers.<name>.url",
    /// ...)` and `get_prop("mcp.servers.<name>.transport")` resolve to
    /// the matching element's own `set_prop` / `get_prop`, and the
    /// `mcp.servers` section behaves like a `HashMap<String,
    /// McpServerConfig>` from the dashboard / TUI's point of view.
    /// `name` itself becomes read-only via `set_prop`; rename goes
    /// through `rename_map_key` which mutates the `name` field in place.
    #[tab(Servers)]
    #[serde(default, alias = "mcpServers")]
    #[nested]
    #[natural_key = "name"]
    pub servers: Vec<McpServerConfig>,
}

fn default_mcp_enabled() -> bool {
    true
}

fn default_deferred_loading() -> bool {
    false
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: default_mcp_enabled(),
            deferred_loading: default_deferred_loading(),
            servers: Vec::new(),
        }
    }
}

/// Verifiable Intent (VI) credential issuance and constraint checking
/// (`[verifiable_intent]` section).
///
/// ZeroClaw implements issuance, crypto, types and constraint checking, but not
/// a credential chain verifier. Until one exists the `vi_verify` tool is
/// withheld from the model-visible registry, so neither key below enables
/// verification of a credential. The library paths are unaffected.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "verifiable_intent"]
pub struct VerifiableIntentConfig {
    /// Opt in to the VI section (default: false).
    ///
    /// While the tool is withheld this does not enable credential verification
    /// on commerce tool calls. It currently causes a warning naming that gap,
    /// emitted once per config application: at process startup, and again when
    /// a daemon reload re-reads config from disk.
    #[serde(default)]
    pub enabled: bool,

    /// Intended strictness mode for constraint evaluation.
    ///
    /// Accepts `"strict"` or `"permissive"`, and defaults to `"strict"`. No
    /// production code reads it while the tool is withheld.
    #[serde(default = "default_vi_strictness")]
    pub strictness: String,
}

fn default_vi_strictness() -> String {
    "strict".to_owned()
}

impl Default for VerifiableIntentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strictness: default_vi_strictness(),
        }
    }
}

// ── Nodes (Dynamic Node Discovery) ───────────────────────────────

fn default_mdns_announce_interval_secs() -> u64 {
    30
}

fn default_mdns_peer_ttl_secs() -> u64 {
    90
}

fn default_mdns_max_peers() -> usize {
    16
}

/// Configuration for LAN-local mDNS peer discovery (`[nodes.mdns]`).
///
/// This config controls only discovery behavior. The advertised gateway
/// endpoint is derived from the running gateway's actual host, port, and path
/// prefix at startup so `[nodes.mdns]` does not duplicate gateway listen state.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "nodes.mdns"]
pub struct MdnsConfig {
    /// Enable mDNS local peer discovery.
    #[serde(default)]
    pub enabled: bool,
    /// Human-readable node name advertised to LAN peers. Defaults to a stable
    /// local fallback when unset.
    #[serde(default)]
    pub node_name: Option<String>,
    /// Maximum number of unauthenticated LAN peer hints retained in memory.
    #[serde(default = "default_mdns_max_peers")]
    pub max_peers: usize,
    /// How often this node re-broadcasts its presence, in seconds.
    #[serde(default = "default_mdns_announce_interval_secs")]
    pub announce_interval_secs: u64,
    /// Seconds after the last announcement before a peer is evicted.
    #[serde(default = "default_mdns_peer_ttl_secs")]
    pub peer_ttl_secs: u64,
}

impl Default for MdnsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_name: None,
            max_peers: default_mdns_max_peers(),
            announce_interval_secs: default_mdns_announce_interval_secs(),
            peer_ttl_secs: default_mdns_peer_ttl_secs(),
        }
    }
}

/// Configuration for the dynamic node discovery system (`[nodes]`).
///
/// When enabled, external processes/devices can connect via WebSocket
/// at `/ws/nodes` and advertise their capabilities at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "nodes"]
pub struct NodesConfig {
    /// Enable dynamic node discovery endpoint.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of concurrent node connections.
    #[serde(default = "default_max_nodes")]
    pub max_nodes: usize,
    /// Optional bearer token for node authentication.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub auth_token: Option<String>,
    /// LAN-local mDNS peer discovery.
    #[serde(default)]
    #[nested]
    pub mdns: MdnsConfig,
}

fn default_max_nodes() -> usize {
    16
}

impl Default for NodesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_nodes: default_max_nodes(),
            auth_token: None,
            mdns: MdnsConfig::default(),
        }
    }
}

// ── TTS (Text-to-Speech) ─────────────────────────────────────────

fn default_tts_voice() -> String {
    "alloy".into()
}

fn default_tts_format() -> String {
    "mp3".into()
}

fn default_tts_max_text_length() -> usize {
    4096
}

/// Text-to-Speech subsystem configuration (`[tts]`).
///
/// Per-instance TTS configs live under `[tts_providers.<type>.<alias>]`
/// (parallel to `providers.models`). What remains here are the global
/// runtime knobs that apply to every model_provider invocation.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tts"]
pub struct TtsConfig {
    /// Enable TTS synthesis.
    #[serde(default)]
    pub enabled: bool,
    /// Default voice ID passed to the selected tts provider.
    #[serde(default = "default_tts_voice")]
    pub default_voice: String,
    /// Default audio output format (`"mp3"`, `"opus"`, `"wav"`).
    #[serde(default = "default_tts_format")]
    pub default_format: String,
    /// Maximum input text length in characters (default 4096).
    #[serde(default = "default_tts_max_text_length")]
    pub max_text_length: usize,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_voice: default_tts_voice(),
            default_format: default_tts_format(),
            max_text_length: default_tts_max_text_length(),
        }
    }
}

/// Per-instance TTS model_provider configuration (`[tts_providers.<type>.<alias>]`).
///
/// Mirrors `ModelProviderConfig` in shape — one struct holds the union of
/// fields across backends. Only the fields relevant to the selected backend
/// (determined by the outer `<type>` map key) are read at runtime; others
/// are quietly ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tts_provider"]
#[serde(default)]
pub struct TtsProviderConfig {
    /// API key (openai, elevenlabs, google).
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Model name. OpenAI uses this for `tts-1`/`tts-1-hd`; elevenlabs uses
    /// it as the model_id (e.g. `eleven_monolingual_v1`).
    pub model: Option<String>,
    /// Voice override for this instance. When empty, falls back to
    /// `[tts].default_voice`.
    pub voice: Option<String>,
    /// Playback speed multiplier (openai only; default `1.0`).
    pub speed: Option<f64>,
    /// Voice stability for elevenlabs (0.0-1.0; default `0.5`).
    pub stability: Option<f64>,
    /// Similarity boost for elevenlabs (0.0-1.0; default `0.5`).
    pub similarity_boost: Option<f64>,
    /// Language code for google (e.g. `en-US`).
    pub language_code: Option<String>,
    /// Path to backend binary (edge-tts subprocess; piper local server).
    pub binary_path: Option<String>,
    /// Audio response format sent to the TTS backend (e.g. `"opus"`, `"mp3"`,
    /// `"wav"`). Defaults to `"opus"` for the OpenAI family. Override to
    /// `"wav"` for Orpheus-class models (e.g. `canopylabs/orpheus-v1-english`
    /// on Groq) or `"mp3"` for broader compatibility.
    pub response_format: Option<String>,
    /// Endpoint URI for HTTP-based backends. Overrides the family default
    /// when pointing at a compatible third-party API (Groq, Azure, self-hosted
    /// proxies). Set to the **full** URL — there is no separate path-suffix
    /// field. Renamed from `api_url` for parity with `ModelProviderConfig.uri`.
    #[serde(alias = "api_url")]
    pub uri: Option<String>,
}

// ── TTS endpoint trait + per-family typed configs ──────────────────────────
//
// Mirrors the model provider typed-family pattern. Each TTS family carries
// its own typed config (composing TtsProviderConfig as the shared base via
// `#[serde(flatten)]`) and a single-variant `*TtsEndpoint` enum impl'ing
// `TtsEndpoint`. Edge and Piper skip the base — they're subprocess / local
// runtimes with no shared `api_key` / `voice` defaults.

/// One trait per family-endpoint enum. Returns the URI for the chosen
/// variant. Mirrors `ModelEndpoint` for parity across model and TTS.
pub trait TtsEndpoint {
    fn uri(&self) -> &'static str;
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenAITtsEndpoint {
    #[default]
    Default,
}
impl TtsEndpoint for OpenAITtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.openai.com/v1/audio/speech",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.openai"]
pub struct OpenAITtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ElevenLabsTtsEndpoint {
    #[default]
    Default,
}
impl TtsEndpoint for ElevenLabsTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.elevenlabs.io/v1/text-to-speech",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.elevenlabs"]
pub struct ElevenLabsTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GoogleTtsEndpoint {
    #[default]
    Default,
}
impl TtsEndpoint for GoogleTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://texttospeech.googleapis.com/v1/text:synthesize",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.google"]
pub struct GoogleTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum EdgeTtsEndpoint {
    /// Subprocess — no remote endpoint. Sentinel for trait conformity.
    #[default]
    LocalSubprocess,
}
impl TtsEndpoint for EdgeTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalSubprocess => "subprocess://edge-tts",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.edge"]
pub struct EdgeTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PiperTtsEndpoint {
    #[default]
    LocalDefault,
}
impl TtsEndpoint for PiperTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://127.0.0.1:5000/v1/audio/speech",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.piper"]
pub struct PiperTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

// ── Transcription providers (typed-family split, mirrors models/tts) ────
//
// Six family slots: `groq`, `openai`, `deepgram`, `assemblyai`, `google`,
// `local_whisper`. Each is a `HashMap<String, *TranscriptionProviderConfig>`
// keyed by operator-chosen alias. The shared `TranscriptionProviderConfig`
// base carries `api_key` + `language` since every cloud STT family takes
// both; `local_whisper` skips the base because it's a self-hosted endpoint
// with its own auth token, not a vendor API key.

/// Shared base for cloud transcription providers. Each cloud family
/// composes this via `#[serde(flatten)] base: TranscriptionProviderConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription"]
pub struct TranscriptionProviderConfig {
    /// API key for the transcription provider.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Optional language hint passed to the provider (ISO-639-1 like `"en"` /
    /// `"ru"`, or BCP-47 like `"en-US"` for Google). Most providers auto-detect
    /// when this is unset.
    #[serde(default)]
    pub language: Option<String>,
    /// Whisper-style initial prompt to bias the model toward expected
    /// vocabulary (proper nouns, technical terms). Provider-specific support;
    /// silently ignored where not applicable.
    #[serde(default)]
    pub initial_prompt: Option<String>,
}

/// Trait that every transcription endpoint enum implements. Mirrors
/// `ModelEndpoint` / `TtsEndpoint` for parity.
pub trait TranscriptionEndpoint {
    fn uri(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GroqTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for GroqTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.groq.com/openai/v1/audio/transcriptions",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.groq"]
pub struct GroqTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
    /// Whisper model name (default: `"whisper-large-v3-turbo"`).
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenAiTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for OpenAiTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.openai.com/v1/audio/transcriptions",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.openai"]
pub struct OpenAiTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
    /// Whisper model name (default: `"whisper-1"`).
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeepgramTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for DeepgramTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.deepgram.com/v1/listen",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.deepgram"]
pub struct DeepgramTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
    /// Deepgram model name (default: `"nova-2"`).
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AssemblyAiTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for AssemblyAiTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.assemblyai.com/v2/transcript",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.assemblyai"]
pub struct AssemblyAiTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GoogleTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for GoogleTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://speech.googleapis.com/v1/speech:recognize",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.google"]
pub struct GoogleTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LocalWhisperTranscriptionEndpoint {
    /// Self-hosted endpoint — no remote URL. Sentinel for trait conformity.
    /// The actual URL lives on `LocalWhisperTranscriptionProviderConfig.uri`.
    #[default]
    SelfHosted,
}
impl TranscriptionEndpoint for LocalWhisperTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::SelfHosted => "self-hosted",
        }
    }
}

/// Local / self-hosted Whisper-compatible transcription endpoint. Skips the
/// shared `TranscriptionProviderConfig` base because it uses a bearer-token
/// scheme and a per-instance URL rather than a vendor API key.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.local_whisper"]
pub struct LocalWhisperTranscriptionProviderConfig {
    /// Endpoint URL, e.g. `"http://10.10.0.1:8001/v1/transcribe"`.
    pub uri: String,
    /// Bearer token for endpoint authentication. Omit for unauthenticated
    /// local endpoints.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bearer_token: Option<String>,
    /// Optional language hint (passed through to the local endpoint).
    #[serde(default)]
    pub language: Option<String>,
    /// Maximum audio file size in bytes accepted by this endpoint.
    /// Defaults to 25 MB to match the cloud cap; raise as needed.
    #[serde(default = "default_local_whisper_max_audio_bytes")]
    pub max_audio_bytes: usize,
    /// Request timeout in seconds.
    #[serde(default = "default_local_whisper_timeout_secs")]
    pub timeout_secs: u64,
}

// `#[derive(Default)]` would leave `max_audio_bytes = 0` and `timeout_secs = 0`
// (Rust's `usize`/`u64` defaults), even though those fields have serde defaults
// pointing at `default_local_whisper_max_audio_bytes` /
// `default_local_whisper_timeout_secs`. `#[serde(default = ...)]` only fires for
// *deserialization* — Rust's `Default::default()` bypasses it.
//
// Without this manual impl, the `Configurable` macro-generated
// `create_map_key(...)` path inserts `LocalWhisperTranscriptionProviderConfig::default()`
// for any newly scaffolded `[providers.transcription.local_whisper.<alias>]`
// entry. The `Configurable` macro exposes `HashMap<String, T>` sections through
// `map_key_sections()` / `create_map_key()`, and the generated create path
// inserts `<T as Default>::default()` for new map entries — leaving
// `max_audio_bytes = 0`, `timeout_secs = 0`. `LocalWhisperProvider::from_typed_config`
// bridges those fields into `LocalWhisperProvider::from_config`, which still
// rejects zero for both fields. Defaults here mirror `LocalWhisperConfig` so
// scaffolded entries load without operator intervention.
impl Default for LocalWhisperTranscriptionProviderConfig {
    fn default() -> Self {
        Self {
            uri: String::new(),
            bearer_token: None,
            language: None,
            max_audio_bytes: default_local_whisper_max_audio_bytes(),
            timeout_secs: default_local_whisper_timeout_secs(),
        }
    }
}

/// Determines when a `ToolFilterGroup` is active.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ToolFilterGroupMode {
    /// Tools in this group are always included in every turn.
    Always,
    /// Tools in this group are included only when the user message contains
    /// at least one of the configured `keywords` (case-insensitive substring match).
    #[default]
    Dynamic,
}

/// A named group of MCP tool patterns with an activation mode.
///
/// Each group lists glob patterns for MCP tool names and an optional set of
/// keywords that trigger inclusion in `dynamic` mode. MCP tools are named
/// `<server>__<tool>` (double-underscore separator, e.g. `filesystem__read_file`).
///
/// Classification is by origin, not name shape: only tools that actually came
/// from the MCP registry are subject to these groups. Built-in tools AND skill
/// tools — which share the `<x>__<y>` naming convention (`{skill}__{tool}`) —
/// always pass through and are never affected by `tool_filter_groups`.
///
/// Under `[mcp] deferred_loading = true`, `mode = "always"` entries are
/// pre-activated at assembly time so matching tools are live from the first
/// turn without a `tool_search` round-trip.
///
/// # Example
/// ```toml
/// [[runtime_profiles.default.tool_filter_groups]]
/// mode = "always"
/// tools = ["filesystem__*"]
/// keywords = []
///
/// [[runtime_profiles.default.tool_filter_groups]]
/// mode = "dynamic"
/// tools = ["browser__*"]
/// keywords = ["browse", "website", "url", "search"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ToolFilterGroup {
    /// Activation mode: `"always"` or `"dynamic"`.
    #[serde(default)]
    pub mode: ToolFilterGroupMode,
    /// Glob patterns matching MCP tool names (single `*` wildcard supported).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Keywords that activate this group in `dynamic` mode (case-insensitive substring).
    /// Ignored when `mode = "always"`.
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// OpenAI Whisper STT model_provider configuration (`[transcription.openai]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.openai"]
pub struct OpenAiSttConfig {
    /// OpenAI API key for Whisper transcription.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Whisper model name (default: "whisper-1").
    #[serde(default = "default_openai_stt_model")]
    pub model: String,
}

/// Deepgram STT model_provider configuration (`[transcription.deepgram]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.deepgram"]
pub struct DeepgramSttConfig {
    /// Deepgram API key.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Deepgram model name (default: "nova-2").
    #[serde(default = "default_deepgram_stt_model")]
    pub model: String,
}

/// AssemblyAI STT model_provider configuration (`[transcription.assemblyai]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.assemblyai"]
pub struct AssemblyAiSttConfig {
    /// AssemblyAI API key.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
}

/// Google Cloud Speech-to-Text model_provider configuration (`[transcription.google]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.google"]
pub struct GoogleSttConfig {
    /// Google Cloud API key.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// BCP-47 language code (default: "en-US").
    #[serde(default = "default_google_stt_language_code")]
    pub language_code: String,
}

/// Local/self-hosted Whisper-compatible STT endpoint (`[transcription.local_whisper]`).
///
/// Configures a self-hosted STT endpoint. Can be on localhost, a private network host, or any reachable URL.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.local_whisper"]
pub struct LocalWhisperConfig {
    /// HTTP or HTTPS endpoint URL, e.g. `"http://10.10.0.1:8001/v1/transcribe"`.
    pub url: String,
    /// Bearer token for endpoint authentication.
    /// Omit for unauthenticated local endpoints.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bearer_token: Option<String>,
    /// Maximum audio file size in bytes accepted by this endpoint.
    /// Defaults to 25 MB — matching the cloud API cap for a safe out-of-the-box
    /// experience. Self-hosted endpoints can accept much larger files; raise this
    /// as needed, but note that each transcription call clones the audio buffer
    /// into a multipart payload, so peak memory per request is ~2× this value.
    #[serde(default = "default_local_whisper_max_audio_bytes")]
    pub max_audio_bytes: usize,
    /// Request timeout in seconds. Defaults to 300 (large files on local GPU).
    #[serde(default = "default_local_whisper_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_local_whisper_max_audio_bytes() -> usize {
    25 * 1024 * 1024
}

fn default_local_whisper_timeout_secs() -> u64 {
    300
}

// `#[derive(Default)]` would leave `max_audio_bytes = 0` and `timeout_secs = 0`
// (Rust's `usize`/`u64` defaults), even though those fields have serde defaults
// pointing at the helpers above. `#[serde(default = ...)]` only fires for
// *deserialization* — Rust's `Default::default()` bypasses it. Without this
// manual impl, `Config::init_defaults` materializes
// `transcription.local_whisper = Some(LocalWhisperConfig { max_audio_bytes: 0,
// timeout_secs: 0, .. })`; `LocalWhisperProvider::from_config` then rejects it
// at load (`max_audio_bytes must be greater than zero`), the failure poisons
// the parent `[transcription]` block's deserialization, and the daemon logs
// `dropped_config: transcription` while running with `transcription.enabled =
// false` regardless of operator intent.
impl Default for LocalWhisperConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            bearer_token: None,
            max_audio_bytes: default_local_whisper_max_audio_bytes(),
            timeout_secs: default_local_whisper_timeout_secs(),
        }
    }
}

/// HMAC tool execution receipt configuration, per agent
/// (`[agents.<alias>.tool_receipts]`).
///
/// Receipts are short HMAC-SHA256 tags appended to tool results so the model
/// cannot claim it ran a tool that never actually executed. See
/// `docs/book/src/security/tool-receipts.md`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "delegate_agent.tool_receipts"]
pub struct ToolReceiptsConfig {
    /// Generate HMAC receipts on every tool execution. Default: `false`.
    /// When false, the entire receipt subsystem is inert (no key, no
    /// generation, no append, no system-prompt addendum).
    #[serde(default)]
    pub enabled: bool,
    /// Append a trailing `Tool receipts:` block to user-visible replies so
    /// receipts are auditable from the channel surface, not just the
    /// internal history. Default: `false`.
    #[serde(default)]
    pub show_in_response: bool,
    /// Inject the receipt-echo instruction into the system prompt so the
    /// model carries receipts verbatim into its response. Default: `true`.
    /// No effect when `enabled = false`.
    #[serde(default = "default_inject_system_prompt")]
    pub inject_system_prompt: bool,
}

fn default_inject_system_prompt() -> bool {
    true
}

impl Default for ToolReceiptsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            show_in_response: false,
            inject_system_prompt: default_inject_system_prompt(),
        }
    }
}

fn default_max_tool_result_chars() -> usize {
    50_000
}

fn default_keep_tool_context_turns() -> usize {
    2
}

fn default_agent_tool_dispatcher() -> String {
    "auto".into()
}

fn default_max_system_prompt_chars() -> usize {
    0
}

// ── Pacing ────────────────────────────────────────────────────────

/// Pacing controls for slow/local LLM workloads (`[pacing]` section).
///
/// All fields are optional and default to values that preserve existing
/// behavior. When set, they extend — not replace — the existing timeout
/// and loop-detection subsystems.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "pacing"]
pub struct PacingConfig {
    /// Per-step timeout in seconds: the maximum time allowed for a single
    /// LLM inference turn, independent of the total message budget.
    /// `None` means no per-step timeout (existing behavior).
    #[serde(default)]
    pub step_timeout_secs: Option<u64>,

    /// Minimum elapsed seconds before the additional identical-successful-round
    /// advisory starts counting. `None` disables this extra advisory. This is
    /// an observation grace period, not a timeout; ordinary repetition and
    /// failed-call recovery remain active when loop detection is enabled.
    #[serde(default)]
    pub loop_detection_min_elapsed_secs: Option<u64>,

    /// Tool names excluded from identical-output / alternating-pattern loop
    /// detection. Useful for browser workflows where `browser_screenshot`
    /// structurally resembles a loop even when making progress.
    #[serde(default)]
    pub loop_ignore_tools: Vec<String>,

    /// Override for the hardcoded timeout scaling cap (default: 4).
    /// The channel message timeout budget is computed as:
    ///   `message_timeout_secs * min(max_tool_iterations, message_timeout_scale_max)`
    /// Raising this value lets long multi-step tasks with slow local models
    /// receive a proportionally larger budget without inflating the base timeout.
    #[serde(default)]
    pub message_timeout_scale_max: Option<u64>,

    /// Enable repetition detection and failed-call recovery. Successful exact
    /// repeats, ping-pong and no-progress patterns are advisory; only another
    /// identical failure after a recovery warning closes the turn. Defaults
    /// to `true`.
    #[serde(default = "default_loop_detection_enabled")]
    pub loop_detection_enabled: bool,

    /// Sliding window size for the pattern-based loop detector.
    /// Defaults to 20.
    #[serde(default = "default_loop_detection_window_size")]
    pub loop_detection_window_size: usize,

    /// Number of consecutive identical tool+args+result calls before a warning.
    /// Defaults to 3. Successful calls always require at least two observations.
    #[serde(default = "default_loop_detection_max_repeats")]
    pub loop_detection_max_repeats: usize,
}

fn default_loop_detection_enabled() -> bool {
    true
}

fn default_loop_detection_window_size() -> usize {
    20
}

fn default_loop_detection_max_repeats() -> usize {
    3
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            step_timeout_secs: None,
            loop_detection_min_elapsed_secs: None,
            loop_ignore_tools: Vec::new(),
            message_timeout_scale_max: None,
            loop_detection_enabled: default_loop_detection_enabled(),
            loop_detection_window_size: default_loop_detection_window_size(),
            loop_detection_max_repeats: default_loop_detection_max_repeats(),
        }
    }
}

/// Skills loading configuration (`[skills]` section).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SkillsPromptInjectionMode {
    /// Inline full skill instructions and tool metadata into the system prompt.
    #[default]
    Full,
    /// Inline only compact skill metadata (name/description/location) and load details on demand.
    Compact,
}

/// Skills loading configuration (`[skills]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "skills"]
pub struct SkillsConfig {
    /// Allow script-like files in skills (`.sh`, `.bash`, `.ps1`, shebang shell files).
    /// Default: `false` (secure by default).
    #[serde(default)]
    pub allow_scripts: bool,
    /// Controls how skills are injected into the system prompt.
    /// `full` preserves legacy behavior. `compact` keeps context small and loads skills on demand.
    #[serde(default)]
    pub prompt_injection_mode: SkillsPromptInjectionMode,
}

/// Multimodal (image) handling configuration (`[multimodal]` section).
///
/// # Privacy and cost note
///
/// Tool results that print real local image paths (e.g. shell tools doing
/// `ls /pictures` or `find . -name '*.png'`) are canonicalized into
/// `[IMAGE:...]` markers and base64-inlined into the next provider request.
/// This means image bytes that previously stayed local will be uploaded to
/// the configured provider when surfaced by a tool.
///
/// `max_images` (and the `trim_old_images` LRU policy) bounds the per-request
/// image budget, but operators running shell-style tools over directories of
/// personal or sensitive images should be aware of the upload semantics. See
/// `docs/book/src/contributing/privacy.md` for the project's privacy stance.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "multimodal"]
pub struct MultimodalConfig {
    /// Maximum number of image attachments accepted per request.
    ///
    /// Caps the total number of `[IMAGE:...]` markers that survive into the
    /// provider request after multimodal preprocessing. Older images are
    /// dropped first when the cumulative count exceeds this limit. Acts as
    /// the upper bound on per-turn upload cost when tool outputs surface
    /// local image paths.
    #[serde(default = "default_multimodal_max_images")]
    pub max_images: usize,
    /// Maximum image payload size in MiB before base64 encoding.
    #[serde(default = "default_multimodal_max_image_size_mb")]
    pub max_image_size_mb: usize,
    /// Maximum age of images in conversation turns.
    ///
    /// When non-zero, images in user messages that are more than this many
    /// turns back from the end of history are stripped before the request is
    /// sent to the provider. This prevents a single screenshot from being
    /// re-encoded and re-uploaded on every subsequent turn indefinitely.
    /// Tool-result images are already managed by the stale-tool-result
    /// mechanism and are not affected by this setting.
    ///
    /// `0` (the default) disables age-based trimming entirely — images are
    /// only evicted by the `max_images` count cap.
    #[serde(default)]
    pub max_image_turns: usize,
    /// Allow fetching remote image URLs (http/https). Disabled by default.
    #[serde(default)]
    pub allow_remote_fetch: bool,
    /// ModelProvider name to use for vision/image messages (e.g. `"ollama"`).
    /// When set, messages containing `[IMAGE:]` markers are routed to this
    /// model_provider instead of the default text model_provider.
    #[serde(default)]
    pub vision_model_provider: Option<String>,
    /// Model to use when routing to the vision model_provider (e.g. `"llava:7b"`).
    /// Only used when `vision_model_provider` is set.
    #[serde(default)]
    pub vision_model: Option<String>,
}

fn default_multimodal_max_images() -> usize {
    4
}

fn default_multimodal_max_image_size_mb() -> usize {
    5
}

impl MultimodalConfig {
    /// Clamp configured values to safe runtime bounds.
    pub fn effective_limits(&self) -> (usize, usize) {
        let max_images = self.max_images.clamp(1, 16);
        let max_image_size_mb = self.max_image_size_mb.clamp(1, 20);
        (max_images, max_image_size_mb)
    }
}

impl Default for MultimodalConfig {
    fn default() -> Self {
        Self {
            max_images: default_multimodal_max_images(),
            max_image_size_mb: default_multimodal_max_image_size_mb(),
            max_image_turns: 0,
            allow_remote_fetch: false,
            vision_model_provider: None,
            vision_model: None,
        }
    }
}

// ── Identity (AIEOS / OpenClaw format) ──────────────────────────

/// Identity format configuration (`[identity]` section).
///
/// Supports `"openclaw"` (default) or `"aieos"` identity documents.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "identity"]
pub struct IdentityConfig {
    /// Identity format: "openclaw" (default) or "aieos"
    #[serde(default = "default_identity_format")]
    pub format: String,
    /// Path to AIEOS JSON file (relative to workspace)
    #[serde(default)]
    pub aieos_path: Option<String>,
    /// Inline AIEOS JSON (alternative to file path)
    #[serde(default)]
    pub aieos_inline: Option<String>,
}

fn default_identity_format() -> String {
    "openclaw".into()
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            format: default_identity_format(),
            aieos_path: None,
            aieos_inline: None,
        }
    }
}

// ── Cost tracking and budget enforcement ───────────────────────────

/// Cost tracking and budget enforcement configuration (`[cost]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost"]
pub struct CostConfig {
    /// Enable cost tracking (default: true)
    #[tab(Limits)]
    #[serde(default = "default_cost_enabled")]
    pub enabled: bool,

    /// Daily spending limit in USD (default: 10.00)
    #[tab(Limits)]
    #[serde(default = "default_daily_limit")]
    pub daily_limit_usd: f64,

    /// Monthly spending limit in USD (default: 100.00)
    #[tab(Limits)]
    #[serde(default = "default_monthly_limit")]
    pub monthly_limit_usd: f64,

    /// Warn when spending reaches this percentage of limit (default: 80)
    #[tab(Limits)]
    #[serde(default = "default_warn_percent")]
    pub warn_at_percent: u8,

    /// Allow requests to exceed budget with --override flag (default: false)
    #[tab(Limits)]
    #[serde(default)]
    pub allow_override: bool,

    /// Cost enforcement behavior when budget limits are approached or exceeded.
    #[tab(Limits)]
    #[serde(default)]
    #[nested]
    pub enforcement: CostEnforcementConfig,

    /// Stamp each recorded cost entry with the originating agent alias so
    /// `/api/cost?agent=<alias>` and CLI rollups can attribute spend to a
    /// specific agent. Disable on high-volume deployments if the extra
    /// HashMap aggregation shows up in profiles (default: true).
    #[tab(Limits)]
    #[serde(default = "default_track_per_agent")]
    pub track_per_agent: bool,

    /// Operator-managed rate sheet at `[cost.rates.*]`. Sections mirror
    /// the `[providers.*]` dotted-path exactly with the trailing `alias`
    /// segment replaced by the resource the rate applies to (model id,
    /// tool name, …). Layout:
    ///
    /// ```toml
    /// [cost.rates.providers.models.anthropic."claude-opus-4-7"]
    /// input_per_mtok = 15.0
    /// output_per_mtok = 75.0
    /// cached_input_per_mtok = 1.5
    ///
    /// [cost.rates.providers.tts.openai."tts-1-hd"]
    /// per_mchar = 30.0
    ///
    /// [cost.rates.providers.transcription.openai.whisper-1]
    /// per_minute = 0.006
    ///
    /// [cost.rates.tools.web_search]
    /// per_call = 0.005
    /// ```
    #[tab(Costs)]
    #[serde(default)]
    #[nested]
    pub rates: CostRatesConfig,
}

/// Configuration for cost enforcement behavior when budget limits are reached.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.enforcement"]
pub struct CostEnforcementConfig {
    /// Enforcement mode: "warn", "block", or "route_down".
    #[serde(default = "default_cost_enforcement_mode")]
    pub mode: String,
    /// Model hint to route to when budget is exceeded (used with "route_down" mode).
    #[serde(default)]
    pub route_down_model: Option<String>,
    /// Reserve this percentage of budget for critical operations.
    #[serde(default = "default_reserve_percent")]
    pub reserve_percent: u8,
}

fn default_cost_enforcement_mode() -> String {
    "warn".to_string()
}

fn default_reserve_percent() -> u8 {
    10
}

impl Default for CostEnforcementConfig {
    fn default() -> Self {
        Self {
            mode: default_cost_enforcement_mode(),
            route_down_model: None,
            reserve_percent: default_reserve_percent(),
        }
    }
}

fn default_daily_limit() -> f64 {
    10.0
}

fn default_monthly_limit() -> f64 {
    100.0
}

fn default_warn_percent() -> u8 {
    80
}

fn default_cost_enabled() -> bool {
    true
}

fn default_track_per_agent() -> bool {
    true
}

/// `[cost.rates]` — top-level rate-sheet namespace. Mirrors the
/// `[providers.*]` shape so each subsection here points at the same
/// kind of resource its `[providers.*]` counterpart configures.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates"]
pub struct CostRatesConfig {
    /// `[cost.rates.providers.*]` — rates for everything under
    /// `[providers.*]` (models, TTS, transcription, …).
    #[serde(default)]
    #[nested]
    pub providers: ProviderCostRates,

    /// `[cost.rates.tools.<name>]` — per-call rates for tools that
    /// hit paid APIs. Keyed by the tool's registered name.
    #[serde(default)]
    #[nested]
    #[resource_key]
    pub tools: std::collections::HashMap<String, ToolCostRates>,
}

impl CostRatesConfig {
    /// Lookup model token rates by `(provider_type, model)`. Dispatch
    /// lives on the typed wrapper — see [`crate::providers::ModelCostRatesByProvider`].
    #[must_use]
    pub fn model_rates(&self, provider_type: &str, model: &str) -> Option<&ModelCostRates> {
        self.providers.models.get(provider_type, model)
    }

    /// Lookup TTS rates by `(provider_type, voice)`.
    #[must_use]
    pub fn tts_rates(&self, provider_type: &str, voice: &str) -> Option<&TtsCostRates> {
        self.providers.tts.get(provider_type, voice)
    }

    /// Lookup transcription rates by `(provider_type, model)`.
    #[must_use]
    pub fn transcription_rates(
        &self,
        provider_type: &str,
        model: &str,
    ) -> Option<&TranscriptionCostRates> {
        self.providers.transcription.get(provider_type, model)
    }

    /// Lookup tool per-call rate by registered name.
    #[must_use]
    pub fn tool_rates(&self, tool_name: &str) -> Option<&ToolCostRates> {
        self.tools.get(tool_name)
    }
}

/// `[cost.rates.providers.*]` — provider-shaped rate sheets. Each field
/// here mirrors a corresponding field on `[providers.*]` with the
/// trailing alias segment replaced by the resource the rate prices.
/// The inner typed wrappers carry the per-provider-type slot layout
/// and own dispatch (their slot list is the single source of truth,
/// shared with their providers counterpart via the `for_each_*_provider_slot!`
/// macros in [`crate::providers`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers"]
pub struct ProviderCostRates {
    /// `[cost.rates.providers.models.<type>.<model>]`.
    #[serde(default)]
    #[nested]
    pub models: crate::providers::ModelCostRatesByProvider,
    /// `[cost.rates.providers.tts.<type>.<voice>]`.
    #[serde(default)]
    #[nested]
    pub tts: crate::providers::TtsCostRatesByProvider,
    /// `[cost.rates.providers.transcription.<type>.<model>]`.
    #[serde(default)]
    #[nested]
    pub transcription: crate::providers::TranscriptionCostRatesByProvider,
}

/// The provider families that carry `[cost.rates.providers.*]` rate sheets, as
/// the trailing segment shared by their `[providers.<category>]` section. One
/// entry per `#[nested]` field on `ProviderCostRates`; this list is the registry
/// the TUI and web surfaces resolve cost tabs from, never a per-surface literal.
pub const PROVIDER_COST_CATEGORIES: [&str; 3] = ["models", "tts", "transcription"];

/// Cost-rate category for a `providers.<category>` section key, or `None` for
/// sections that carry no rate sheets. Resolves against
/// [`PROVIDER_COST_CATEGORIES`] so a new rate-bearing family is added in one
/// place.
#[must_use]
pub fn cost_category_for_provider_section(section_key: &str) -> Option<&'static str> {
    let suffix = section_key.strip_prefix("providers.")?;
    PROVIDER_COST_CATEGORIES
        .into_iter()
        .find(|category| *category == suffix)
}

/// Token-cost rates for a single chat / completion model, in USD per
/// 1M tokens. Every field optional so partial sheets work without
/// ceremony (an operator who only knows the input rate can record it).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers.models"]
pub struct ModelCostRates {
    /// Input tokens (USD per 1M).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    /// Output tokens (USD per 1M).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    /// Cached input tokens (USD per 1M). Optional — leave unset on
    /// providers that don't charge separately for prompt cache hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
}

/// Rates for a TTS model, in USD per 1M characters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers.tts"]
pub struct TtsCostRates {
    /// Characters synthesised (USD per 1M).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_mchar: Option<f64>,
}

/// Rates for a transcription model, in USD per minute of audio.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers.transcription"]
pub struct TranscriptionCostRates {
    /// Audio transcribed (USD per minute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_minute: Option<f64>,
}

/// Rates for a tool that hits a paid external API. Keyed in
/// `[cost.rates.tools.<name>]` by the tool's registered name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.tools"]
pub struct ToolCostRates {
    /// Per-call cost (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_call: Option<f64>,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            daily_limit_usd: default_daily_limit(),
            monthly_limit_usd: default_monthly_limit(),
            warn_at_percent: default_warn_percent(),
            allow_override: false,
            enforcement: CostEnforcementConfig::default(),
            track_per_agent: default_track_per_agent(),
            rates: CostRatesConfig::default(),
        }
    }
}

// ── Gateway security ─────────────────────────────────────────────

/// Gateway server configuration (`[gateway]` section).
///
/// Controls the HTTP gateway for webhook and pairing endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway"]
#[allow(clippy::struct_excessive_bools)]
pub struct GatewayConfig {
    /// Optional owner attention policy for bridge notifications; absent preserves immediate timing.
    #[serde(default)]
    #[nested]
    pub attention: Option<crate::attention::AttentionConfig>,
    /// Gateway port (default: 42617)
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Gateway host (default: 127.0.0.1)
    #[serde(default = "default_gateway_host")]
    pub host: String,
    /// Require pairing before accepting requests (default: true)
    #[serde(default = "default_true")]
    pub require_pairing: bool,
    /// Acknowledge a non-loopback bind while pairing is enabled (default: false).
    #[serde(default)]
    pub allow_public_bind: bool,
    /// Dangerously allow network-facing access without pairing (default: false).
    /// Independent of allow_public_bind; reachable clients can control the agent.
    #[serde(default)]
    pub dangerously_allow_unauthenticated_public_bind: bool,
    /// Allow authenticated remote callers to use admin endpoints that are
    /// otherwise localhost-only. Currently this gates `POST /admin/reload`.
    /// When false (default), those endpoints reject any non-loopback peer.
    /// When true, a non-loopback request is accepted only if it also passes
    /// pairing authentication — which requires `require_pairing = true`; with
    /// pairing off a remote caller cannot be authenticated and is rejected, so
    /// this flag never exposes an anonymous remote reload. `/admin/shutdown`
    /// and the pairing-code endpoints stay localhost-only regardless.
    /// (default: false)
    #[serde(default)]
    pub allow_remote_admin: bool,
    /// Paired bearer tokens (managed automatically, not user-edited)
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub paired_tokens: Vec<String>,

    /// Max `/pair` requests per minute per client key.
    #[serde(default = "default_pair_rate_limit")]
    pub pair_rate_limit_per_minute: u32,

    /// Max `/webhook` requests per minute per client key.
    #[serde(default = "default_webhook_rate_limit")]
    pub webhook_rate_limit_per_minute: u32,

    /// Trust proxy-forwarded client IP headers (`X-Forwarded-For`, `X-Real-IP`).
    /// Disabled by default; enable only behind a trusted reverse proxy.
    #[serde(default)]
    #[credential_class = "public_value"]
    pub trust_forwarded_headers: bool,

    /// Optional URL path prefix for reverse-proxy deployments.
    /// When set, all gateway routes are served under this prefix.
    /// Must start with `/` and must not end with `/`.
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Maximum distinct client keys tracked by gateway rate limiter maps.
    #[serde(default = "default_gateway_rate_limit_max_keys")]
    pub rate_limit_max_keys: usize,

    /// TTL for webhook idempotency keys.
    #[serde(default = "default_idempotency_ttl_secs")]
    pub idempotency_ttl_secs: u64,

    /// Maximum distinct idempotency keys retained in memory.
    #[serde(default = "default_gateway_idempotency_max_keys")]
    pub idempotency_max_keys: usize,

    /// Persist gateway WebSocket chat sessions to SQLite. Default: true.
    #[serde(default = "default_true")]
    pub session_persistence: bool,

    /// Auto-archive stale gateway sessions older than N hours. 0 = disabled. Default: 0.
    #[serde(default)]
    pub session_ttl_hours: u32,

    /// Path to the web dashboard `dist` directory. When set, the gateway
    /// serves the compiled frontend from the filesystem instead of requiring
    /// it to be embedded in the binary. Accepts absolute paths or paths
    /// relative to the working directory. When omitted the gateway runs in
    /// API-only mode (no web dashboard) unless auto-detection finds it.
    #[serde(default)]
    pub web_dist_dir: Option<String>,

    /// TLS configuration for the gateway server (`[gateway.tls]`).
    #[serde(default)]
    #[nested]
    pub tls: Option<GatewayTlsConfig>,

    /// HTTP request timeout (seconds) for gateway routes other than the
    /// long-running cron-trigger endpoint. Default: 30s.
    #[serde(default = "default_gateway_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// HTTP request timeout (seconds) for `POST /api/cron/{id}/run`, which
    /// runs jobs synchronously and routinely exceeds the 30s default.
    /// Default: 600s (10 minutes).
    #[serde(default = "default_gateway_long_running_request_timeout_secs")]
    pub long_running_request_timeout_secs: u64,

    /// Poll GitHub for newer releases and show an "update available" indicator
    /// on the dashboard version tag. Read-only; does not install anything.
    /// (default: true)
    #[serde(default = "default_true")]
    pub check_updates: bool,

    /// Allow triggering a self-upgrade (binary swap via `rei update`) from
    /// the dashboard. This is a remote-code-execution-adjacent surface: any
    /// authenticated dashboard user could replace the running binary. Keep off
    /// unless you trust every paired client. (default: false)
    #[serde(default)]
    pub allow_self_upgrade: bool,

    /// Channel bridges allowed to attach (`[gateway.bridges.<name>]`). A
    /// bridge is a separate process (for example `rei-bridge-telegram`)
    /// that relays a messaging platform through `/ws/chat` and receives
    /// proactive messages (cron, heartbeat, `notify`) on `/ws/bridge`.
    /// Mint entries with `rei gateway bridge add <name>`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub bridges: HashMap<String, GatewayBridgeConfig>,

    /// Explicit management credentials (`[gateway.operator]`). Empty preserves
    /// legacy paired-token operator access; nonempty requires these hashes.
    #[serde(default)]
    #[nested]
    pub operator: GatewayOperatorConfig,
}

/// One channel bridge (`[gateway.bridges.<name>]`).
///
/// The bridge authenticates with its own bearer token; only the token's
/// SHA-256 hash is stored. Bridge tokens are never minted by pairing and
/// are not paired tokens: they open `/ws/bridge` and, within the session
/// scope below, `/ws/chat`. Explicit device `tachi_*` grants additionally
/// admit the bounded delegation facade, never management/operator APIs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway.bridges"]
#[serde(default)]
pub struct GatewayBridgeConfig {
    /// Lowercase hex SHA-256 of the bridge's bearer token.
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub token_hash: String,
    /// Chat sessions whose ids start with this prefix may be opened with the
    /// bridge token (for example `tg:` for per-chat sessions).
    pub session_prefix: Option<String>,
    /// Exact chat session ids the bridge token may open (for example
    /// `main`, the owner's shared session). With neither this nor
    /// `session_prefix` set the token opens no chat session.
    pub sessions: Vec<String>,
    /// Opt-in device access. Missing both classes preserves the legacy bridge contract;
    /// present with no operations grants no access. Never permits approvals.
    #[nested]
    pub device: Option<GatewayDeviceBridgeConfig>,
    /// Explicit bounded owner bridge; mutually exclusive with device.
    #[nested]
    pub owner: Option<GatewayOwnerBridgeConfig>,
}

impl GatewayBridgeConfig {
    /// Explicit operation-limited bridge, as opposed to a legacy channel bridge.
    pub fn scoped(&self) -> bool {
        self.device.is_some() || self.owner.is_some()
    }

    /// Whether the bridge token may open the chat session `session_id`.
    pub fn allows_session(&self, session_id: &str) -> bool {
        if self.sessions.iter().any(|s| s == session_id) {
            return true;
        }
        self.session_prefix
            .as_deref()
            .filter(|prefix| !prefix.is_empty())
            .is_some_and(|prefix| session_id.starts_with(prefix))
    }
}

impl GatewayConfig {
    /// The bridge a bearer token belongs to, by constant-time comparison
    /// of the token's hash against every configured `token_hash`.
    pub fn bridge_for_token(&self, token: &str) -> Option<(&str, &GatewayBridgeConfig)> {
        if token.is_empty() {
            return None;
        }
        let hash = crate::tokens::hash_token(token);
        let mut found = None;
        for (name, bridge) in &self.bridges {
            if crate::tokens::constant_time_eq(&hash, &bridge.token_hash.to_ascii_lowercase())
                && found.is_none()
            {
                found = Some((name.as_str(), bridge));
            }
        }
        found
    }
}

fn default_gateway_port() -> u16 {
    42617
}

fn default_gateway_request_timeout_secs() -> u64 {
    30
}

fn default_gateway_long_running_request_timeout_secs() -> u64 {
    600
}

fn default_gateway_host() -> String {
    "127.0.0.1".into()
}

fn default_pair_rate_limit() -> u32 {
    10
}

fn default_webhook_rate_limit() -> u32 {
    60
}

fn default_idempotency_ttl_secs() -> u64 {
    300
}

fn default_gateway_rate_limit_max_keys() -> usize {
    10_000
}

fn default_gateway_idempotency_max_keys() -> usize {
    10_000
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_memory_backend() -> String {
    "memorycore".into()
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            attention: None,
            port: default_gateway_port(),
            host: default_gateway_host(),
            require_pairing: true,
            allow_public_bind: false,
            dangerously_allow_unauthenticated_public_bind: false,
            allow_remote_admin: false,
            paired_tokens: Vec::new(),
            pair_rate_limit_per_minute: default_pair_rate_limit(),
            webhook_rate_limit_per_minute: default_webhook_rate_limit(),
            trust_forwarded_headers: false,
            path_prefix: None,
            rate_limit_max_keys: default_gateway_rate_limit_max_keys(),
            idempotency_ttl_secs: default_idempotency_ttl_secs(),
            idempotency_max_keys: default_gateway_idempotency_max_keys(),
            session_persistence: true,
            session_ttl_hours: 0,
            web_dist_dir: None,
            tls: None,
            request_timeout_secs: default_gateway_request_timeout_secs(),
            long_running_request_timeout_secs: default_gateway_long_running_request_timeout_secs(),
            check_updates: true,
            allow_self_upgrade: false,
            bridges: HashMap::new(),
            operator: GatewayOperatorConfig::default(),
        }
    }
}

/// TLS configuration for the gateway server (`[gateway.tls]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway.tls"]
pub struct GatewayTlsConfig {
    /// Enable TLS for the gateway (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Path to the PEM-encoded server certificate file.
    pub cert_path: String,
    /// Path to the PEM-encoded server private key file.
    pub key_path: String,
    /// Client certificate authentication (mutual TLS) settings.
    #[serde(default)]
    #[nested]
    pub client_auth: Option<GatewayClientAuthConfig>,
}

/// Client certificate authentication (mTLS) configuration (`[gateway.tls.client_auth]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway.tls.client_auth"]
pub struct GatewayClientAuthConfig {
    /// Enable client certificate verification (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Path to the PEM-encoded CA certificate used to verify client certs.
    #[serde(default)]
    pub ca_cert_path: String,
    /// Reject connections that do not present a valid client certificate (default: true).
    #[serde(default = "default_true")]
    pub require_client_cert: bool,
    /// Optional SHA-256 fingerprints for certificate pinning.
    /// When non-empty, only client certs matching one of these fingerprints are accepted.
    #[serde(default)]
    pub pinned_certs: Vec<String>,
}

impl Default for GatewayClientAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ca_cert_path: String::new(),
            require_client_cert: default_true(),
            pinned_certs: Vec::new(),
        }
    }
}

// ── Secrets (encrypted credential store) ────────────────────────

/// Secrets encryption configuration (`[secrets]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "secrets"]
pub struct SecretsConfig {
    /// Enable encryption for API keys and tokens at rest
    #[serde(default = "default_true")]
    #[credential_class = "public_value"]
    pub encrypt: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self { encrypt: true }
    }
}

/// Optional destination binding for one named HTTP authorization secret.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[prefix = "http_auth_secret_policy"]
pub struct HttpAuthSecretPolicy {
    /// Exact canonical hosts (case-insensitive), without ports or wildcards.
    /// Missing preserves legacy behavior; an empty list denies all destinations.
    #[serde(default)]
    pub allowed_hosts: Option<Vec<String>>,
}

// ── HTTP request tool ───────────────────────────────────────────

/// HTTP request tool configuration (`[http_request]` section).
///
/// Domain filtering: `allowed_domains` controls which hosts are reachable (use `["*"]`
/// for all public hosts, which is the default). If `allowed_domains` is empty, all
/// requests are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "http_request"]
pub struct HttpRequestConfig {
    /// Enable `http_request` tool for API interactions
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allowed domains for HTTP requests (exact or subdomain match)
    #[serde(default = "default_allowed_domains_star")]
    pub allowed_domains: Vec<String>,
    /// Maximum response size in bytes (default: 1MB, 0 = unlimited)
    #[serde(default = "default_http_max_response_size")]
    pub max_response_size: usize,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_http_timeout_secs")]
    pub timeout_secs: u64,
    /// Allow requests to private/LAN hosts (RFC 1918, loopback, link-local, .local).
    /// Default: false (deny private hosts for SSRF protection).
    #[serde(default)]
    pub allow_private_hosts: bool,
    /// Private/internal hosts explicitly allowed to bypass SSRF protection.
    /// Exact and subdomain matches are supported; `*` permits all private/local hosts.
    #[serde(default)]
    pub allowed_private_hosts: Vec<String>,
    /// Named authorization secrets for `auth_secret` requests.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub secrets: HashMap<String, String>,
    /// Optional destination policies keyed by the corresponding secret name.
    /// Kept separate from secret values to preserve encryption and redaction.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub auth_secret_policies: HashMap<String, HttpAuthSecretPolicy>,
}

impl Default for HttpRequestConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_domains: vec!["*".into()],
            max_response_size: default_http_max_response_size(),
            timeout_secs: default_http_timeout_secs(),
            allow_private_hosts: false,
            allowed_private_hosts: vec![],
            secrets: HashMap::new(),
            auth_secret_policies: HashMap::new(),
        }
    }
}

fn default_http_max_response_size() -> usize {
    1_000_000 // 1MB
}

fn default_http_timeout_secs() -> u64 {
    30
}

fn default_allowed_domains_star() -> Vec<String> {
    vec!["*".into()]
}

// ── Web fetch ────────────────────────────────────────────────────

/// Web fetch tool configuration (`[web_fetch]` section).
///
/// Fetches web pages and converts HTML to plain text for LLM consumption.
/// Domain filtering: `allowed_domains` controls which hosts are reachable (use `["*"]`
/// for all public hosts). `blocked_domains` takes priority over `allowed_domains`.
/// If `allowed_domains` is empty, all requests are rejected (deny-by-default).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "web_fetch"]
pub struct WebFetchConfig {
    /// Enable `web_fetch` tool for fetching web page content
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allowed domains for web fetch (exact or subdomain match; `["*"]` = all public hosts)
    #[serde(default = "default_web_fetch_allowed_domains")]
    pub allowed_domains: Vec<String>,
    /// Blocked domains (exact or subdomain match; always takes priority over allowed_domains)
    #[serde(default)]
    pub blocked_domains: Vec<String>,
    /// Private/internal hosts allowed to bypass SSRF protection (e.g. `["192.168.1.10", "internal.local"]`).
    /// Exact and subdomain matches are supported. Listing a host skips the
    /// resolved-IP SSRF check for it; a *literal* private/local host (IP literal,
    /// localhost, .local) listed here also bypasses `allowed_domains`. The `*`
    /// wildcard permits any literal private/local host and lets a name already in
    /// `allowed_domains` resolve to a private IP, but it does NOT widen
    /// `allowed_domains` — a non-private host (matched explicitly or via `*`) still
    /// needs `allowed_domains`, so `*` cannot reach an arbitrary public host.
    #[serde(default)]
    pub allowed_private_hosts: Vec<String>,
    /// Maximum response size in bytes (default: 500KB, plain text is much smaller than raw HTML)
    #[serde(default = "default_web_fetch_max_response_size")]
    pub max_response_size: usize,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_web_fetch_timeout_secs")]
    pub timeout_secs: u64,
    /// Firecrawl fallback configuration (`[web_fetch.firecrawl]`)
    #[serde(default)]
    #[nested]
    pub firecrawl: FirecrawlConfig,
}

/// Firecrawl fallback mode: scrape a single page or crawl linked pages.
#[derive(
    Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum FirecrawlMode {
    #[default]
    Scrape,
    /// Reserved for future multi-page crawl support. Accepted in config
    /// deserialization to avoid breaking existing files, but not yet
    /// implemented — `fetch_via_firecrawl` always uses the `/scrape` endpoint.
    Crawl,
}

/// Firecrawl fallback configuration for JS-heavy and bot-blocked sites.
///
/// When enabled, if the standard web fetch fails (HTTP error, empty body, or
/// body shorter than 100 characters suggesting a JS-only page), the tool
/// falls back to the Firecrawl API for stealth content extraction.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "web_fetch.firecrawl"]
pub struct FirecrawlConfig {
    /// Enable Firecrawl fallback
    #[serde(default)]
    pub enabled: bool,
    /// Environment variable name for the Firecrawl API key
    #[serde(default = "default_firecrawl_api_key_env")]
    #[credential_class = "legacy_env_path"]
    pub api_key_env: String,
    /// Firecrawl API base URL
    #[serde(default = "default_firecrawl_api_url")]
    pub api_url: String,
    /// Firecrawl extraction mode
    #[serde(default)]
    pub mode: FirecrawlMode,
}

fn default_firecrawl_api_key_env() -> String {
    "FIRECRAWL_API_KEY".into()
}

fn default_firecrawl_api_url() -> String {
    "https://api.firecrawl.dev/v1".into()
}

impl Default for FirecrawlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: default_firecrawl_api_key_env(),
            api_url: default_firecrawl_api_url(),
            mode: FirecrawlMode::default(),
        }
    }
}

fn default_web_fetch_max_response_size() -> usize {
    500_000 // 500KB
}

fn default_web_fetch_timeout_secs() -> u64 {
    30
}

fn default_web_fetch_allowed_domains() -> Vec<String> {
    vec!["*".into()]
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_domains: vec!["*".into()],
            blocked_domains: vec![],
            allowed_private_hosts: vec![],
            max_response_size: default_web_fetch_max_response_size(),
            timeout_secs: default_web_fetch_timeout_secs(),
            firecrawl: FirecrawlConfig::default(),
        }
    }
}

// ── Shell tool ───────────────────────────────────────────────────

/// Shell tool configuration (`[shell_tool]` section).
///
/// Controls the behaviour of the `shell` execution tool. The main
/// tunable is `timeout_secs` — the maximum wall-clock time a single
/// shell command may run before it is killed.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "shell_tool"]
pub struct ShellToolConfig {
    /// Maximum shell command execution time in seconds (default: 60).
    #[serde(default = "default_shell_tool_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_shell_tool_timeout_secs() -> u64 {
    60
}

impl Default for ShellToolConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_shell_tool_timeout_secs(),
        }
    }
}

// ── Escalation routing ───────────────────────────────────────────

/// Escalation routing configuration (`[escalation]` section).
///
/// Controls which channels receive alert notifications when
/// `escalate_to_human` is called with high or critical urgency.
/// Channels are identified by name (e.g. `"telegram"`, `"slack"`).
/// Alerts are sent best-effort and do not block the escalation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "escalation"]
pub struct EscalationConfig {
    /// Channel names to alert on high/critical escalations (default: empty).
    ///
    /// Each name must match a configured channel. Unrecognised names are
    /// logged at WARN level and skipped.
    #[serde(default)]
    pub alert_channels: Vec<String>,
}

// ── Web search ───────────────────────────────────────────────────

/// Web search tool configuration (`[web_search]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "web_search"]
pub struct WebSearchConfig {
    /// Enable `web_search_tool` for web searches
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Search provider: "duckduckgo" (free), "brave" (requires API key), "tavily" (requires API key), "searxng" (self-hosted), "jina" (requires API key), or "bocha" (Bocha AI, requires API key — Chinese-friendly, <https://open.bochaai.com>)
    #[serde(default = "default_web_search_provider")]
    pub search_provider: String,
    /// Brave Search API key (required if search_provider is "brave")
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub brave_api_key: Option<String>,
    /// Tavily Search API key (required if search_provider is "tavily")
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub tavily_api_key: Option<String>,
    /// Jina AI API key (required if search_provider is "jina")
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub jina_api_key: Option<String>,
    /// Bocha AI Web Search API key (required if search_provider is `"bocha"`). Obtain at <https://open.bochaai.com>.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bocha_api_key: Option<String>,
    /// SearXNG instance URL (required if search_provider is `"searxng"`), e.g. `"https://searx.example.com"`.
    #[serde(default)]
    pub searxng_instance_url: Option<String>,
    /// Maximum results per search (1-10)
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
    /// Request timeout in seconds
    #[serde(default = "default_web_search_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_web_search_provider() -> String {
    "duckduckgo".into()
}

fn default_web_search_max_results() -> usize {
    5
}

fn default_web_search_timeout_secs() -> u64 {
    15
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            search_provider: default_web_search_provider(),
            brave_api_key: None,
            tavily_api_key: None,
            jina_api_key: None,
            bocha_api_key: None,
            searxng_instance_url: None,
            max_results: default_web_search_max_results(),
            timeout_secs: default_web_search_timeout_secs(),
        }
    }
}

// ── Proxy ───────────────────────────────────────────────────────

/// Proxy application scope — determines which outbound traffic uses the proxy.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProxyScope {
    /// Use system environment proxy variables only.
    Environment,
    /// Apply proxy to all ZeroClaw-managed HTTP traffic (default).
    #[default]
    Zeroclaw,
    /// Apply proxy only to explicitly listed service selectors.
    Services,
}

/// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "proxy"]
pub struct ProxyConfig {
    /// Enable proxy support for selected scope.
    #[serde(default)]
    pub enabled: bool,
    /// Proxy URL for HTTP requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub http_proxy: Option<String>,
    /// Proxy URL for HTTPS requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub https_proxy: Option<String>,
    /// Fallback proxy URL for all schemes.
    #[serde(default)]
    pub all_proxy: Option<String>,
    /// No-proxy bypass list. Same format as NO_PROXY.
    #[serde(default)]
    pub no_proxy: Vec<String>,
    /// Proxy application scope.
    #[serde(default)]
    pub scope: ProxyScope,
    /// Service selectors used when scope = "services".
    #[serde(default)]
    pub services: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: Vec::new(),
            scope: ProxyScope::Zeroclaw,
            services: Vec::new(),
        }
    }
}

impl ProxyConfig {
    pub fn supported_service_keys() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_KEYS
    }

    pub fn supported_service_selectors() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_SELECTORS
    }

    pub fn has_any_proxy_url(&self) -> bool {
        normalize_proxy_url_option(self.http_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.https_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.all_proxy.as_deref()).is_some()
    }

    pub fn normalized_services(&self) -> Vec<String> {
        normalize_service_list(self.services.clone())
    }

    pub fn normalized_no_proxy(&self) -> Vec<String> {
        normalize_no_proxy_list(self.no_proxy.clone())
    }

    pub fn should_apply_to_service(&self, service_key: &str) -> bool {
        if !self.enabled {
            return false;
        }

        match self.scope {
            ProxyScope::Environment => false,
            ProxyScope::Zeroclaw => true,
            ProxyScope::Services => {
                let service_key = service_key.trim().to_ascii_lowercase();
                if service_key.is_empty() {
                    return false;
                }

                self.normalized_services()
                    .iter()
                    .any(|selector| service_selector_matches(selector, &service_key))
            }
        }
    }
}

pub fn normalize_proxy_url_option(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn normalize_no_proxy_list(values: Vec<String>) -> Vec<String> {
    normalize_comma_values(values)
}

fn normalize_service_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = normalize_comma_values(values)
        .into_iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn normalize_comma_values(values: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    for value in values {
        for part in value.split(',') {
            let normalized = part.trim();
            if normalized.is_empty() {
                continue;
            }
            output.push(normalized.to_string());
        }
    }
    output.sort_unstable();
    output.dedup();
    output
}

fn service_selector_matches(selector: &str, service_key: &str) -> bool {
    if selector == service_key {
        return true;
    }

    if let Some(prefix) = selector.strip_suffix(".*") {
        return service_key.starts_with(prefix)
            && service_key
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('.'));
    }

    false
}

// ── Memory ───────────────────────────────────────────────────

/// Persistent storage configuration (`[storage]` section).
///
/// Storage is a two-tier alias-keyed map: `[storage.<backend>.<alias>]`,
/// parallel to `[providers.models.<type>.<alias>]`. Each backend has its own typed
/// config struct. `MemoryConfig.backend` carries a dotted reference (`"markdown.notes"`)
/// that resolves to one of these entries via [`Config::resolve_active_storage`], or
/// selects `"memorycore"` (the embedded kernel) / `"none"`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage"]
pub struct StorageConfig {
    /// Markdown storage instances (`[storage.markdown.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub markdown: HashMap<String, MarkdownStorageConfig>,
}

/// Markdown directory storage (`[storage.markdown.<alias>]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage_markdown"]
#[serde(default)]
pub struct MarkdownStorageConfig {
    /// Optional override for the markdown root directory.
    /// When unset, defaults to `<workspace_dir>/memory/`.
    pub directory: Option<String>,
}

/// Search strategy for memory recall.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, reiclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Pure keyword search (FTS5 BM25)
    Bm25,
    /// Pure vector/semantic search
    Embedding,
    /// Weighted combination of keyword + vector (default)
    #[default]
    Hybrid,
}

/// Memory backend configuration (`[memory]` section).
///
/// Controls conversation memory storage, embeddings, hybrid search, and response
/// caching. Backend-specific settings live under `[storage.<backend>.<alias>]`
/// when applicable; this section selects which backend or storage instance to use
/// via `backend`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "memory"]
#[allow(clippy::struct_excessive_bools)]
pub struct MemoryConfig {
    /// Active memory backend or dotted storage reference: `<backend>[.<alias>]`
    /// (e.g. `"memorycore"`, `"markdown.notes"`). Resolves through
    /// `Config.storage.<backend>.<alias>` for configured storage backends, or
    /// selects a built-in backend (`"memorycore"`, `"markdown"`, `"none"`).
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    /// Auto-save what *you* tell ZeroClaw into memory as conversation history — the agent's own replies are not saved. Turn off if you want memory to only hold things you explicitly record via the memory tool.
    #[serde(default = "default_auto_save")]
    pub auto_save: bool,
    /// Run the periodic hygiene pass that archives stale daily/session files and enforces retention windows. Leave on unless you want to manage cleanup yourself.
    #[serde(default = "default_hygiene_enabled")]
    pub hygiene_enabled: bool,
    /// Also extract atomic durable facts from each consolidated turn and store
    /// them as individual Core memories. Default off; the flip is sequenced in
    /// a later phase. MemoryCore-only: enabling this requires the memorycore memory
    /// backend globally and on every agent (validated at config load).
    #[serde(default)]
    pub consolidation_extract_facts: bool,
    /// Move daily/session files to the archive directory after this many days. Keeps the hot working set small without deleting history.
    #[serde(default = "default_archive_after_days")]
    pub archive_after_days: u32,
    /// Delete archived files permanently after this many days. Set high if you need long-term history; set low for privacy / disk-space reasons.
    #[serde(default = "default_purge_after_days")]
    pub purge_after_days: u32,
    /// Delete conversation rows older than this many days (when enforced by the active backend). Age is measured by `updated_at` (last write time). 0 = keep forever.
    #[serde(default = "default_conversation_retention_days")]
    pub conversation_retention_days: u32,
    /// Delete daily memory rows older than this many days from the store. Age is measured by `updated_at` (last write time). 0 = keep forever.
    #[serde(default = "default_zero_retention")]
    pub daily_retention_days: u32,
    /// Delete core memory rows older than this many days from the store. Age is measured by `created_at` (first-write time). Neither recall nor ordinary rewrites refresh `created_at` on upsert, so core retention is an absolute age limit from first write. Set this to a generously large window for durable core memories, or keep 0 = keep forever.
    #[serde(default = "default_zero_retention")]
    pub core_retention_days: u32,
    /// Source of embedding vectors for semantic search. `none` = keyword-only retrieval (no API calls, no vector cost); `openai` = OpenAI's embedding API; `custom:URL` = any OpenAI-compatible embedding endpoint (LiteLLM, local gateway, etc.).
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Embedding model identifier — must match a model your chosen embedding model_provider serves (e.g. `text-embedding-3-small` for OpenAI). Changing this invalidates existing embeddings: the change is detected at startup and stale vectors are cleared automatically; run `rei memory reindex` to re-embed (or set `auto_reindex_on_identity_change`).
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Vector width produced by the embedding model — must match the model's native dimension or vectors won't store correctly. Look up the number on the model_provider's model page.
    #[serde(default = "default_embedding_dims")]
    pub embedding_dimensions: usize,
    /// Automatically re-embed all memories in the background when a change of embedding provider/model/dimensions is detected at startup (after the stale vectors have been cleared). Costs one embedding API call per memory, so it's off by default — leave it off for large stores and run `rei memory reindex` explicitly instead.
    #[serde(default)]
    pub auto_reindex_on_identity_change: bool,
    /// Optional API key for the embedding endpoint. When set, embedding calls use this key instead of inheriting one from the seed model provider — decoupling embeddings from the chat model. Use it when the chat model runs on a provider that carries no usable embedding credential (e.g. an OAuth-only provider) while embeddings keep hitting an `openai`/`custom:` endpoint with their own key. Leave unset to inherit the seed provider's key (backward-compatible default).
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_api_key: Option<String>,
    /// How heavily vector (semantic) similarity counts when `search_mode = hybrid`. Raise toward 1.0 to favor meaning-based matches; lower it to lean on keyword overlap instead.
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    /// How heavily BM25 (keyword) overlap counts when `search_mode = hybrid`. Raise toward 1.0 for exact-term matching; lower it when paraphrases should still score well.
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    /// How memories are retrieved: `bm25` = keyword-only (no embeddings, cheapest); `embedding` = vector similarity only (needs an embedding model_provider); `hybrid` = blended keyword + vector score using the weights above (most robust).
    #[serde(default)]
    pub search_mode: SearchMode,
    /// Minimum hybrid score (0.0–1.0) for a memory to be included in context.
    /// Memories scoring below this threshold are dropped to prevent irrelevant
    /// context from bleeding into conversations. Default: 0.4
    #[serde(default = "default_min_relevance_score")]
    pub min_relevance_score: f64,
    /// Max embedding cache entries before LRU eviction
    #[serde(default = "default_cache_size")]
    pub embedding_cache_size: usize,
    /// Max tokens per chunk for document splitting
    #[serde(default = "default_chunk_size")]
    pub chunk_max_tokens: usize,

    // ── Response Cache (saves tokens on repeated prompts) ──────
    /// Enable LLM response caching to avoid paying for duplicate prompts
    #[serde(default)]
    pub response_cache_enabled: bool,
    /// TTL in minutes for cached responses (default: 60)
    #[serde(default = "default_response_cache_ttl")]
    pub response_cache_ttl_minutes: u32,
    /// Max number of cached responses before LRU eviction (default: 5000)
    #[serde(default = "default_response_cache_max")]
    pub response_cache_max_entries: usize,
    /// Max in-memory hot cache entries for the two-tier response cache (default: 256)
    #[serde(default = "default_response_cache_hot_entries")]
    pub response_cache_hot_entries: usize,

    // ── Retrieval Pipeline ─────────────────────────────────────
    /// Retrieval stages for per-agent recall. Only `"cache"` is active: it
    /// enables an in-process, per-handle hot cache over recall results and is
    /// omitted from the default so recall stays coherent across handles.
    /// `"fts"` and `"vector"` are reserved for when the backend exposes
    /// distinct FTS and vector operations; recall is a single hybrid backend
    /// call today, so those names have no effect (kept for forward compat).
    #[serde(default = "default_retrieval_stages")]
    pub retrieval_stages: Vec<String>,
    /// Candidate pool multiplier over the final recall limit before blend/rerank trimming.
    /// Values must be in 1..=20; runtime also enforces a bounded candidate pool.
    #[serde(default = "default_candidate_multiplier")]
    pub candidate_multiplier: usize,
    /// Enable the recall rerank stage: blend retrieval score with importance
    /// and recency, collapse near-duplicate entries, then trim back to the
    /// recall limit. The advanced strategy below runs when the candidate
    /// count reaches `rerank_threshold`.
    #[serde(default)]
    pub rerank_enabled: bool,
    /// Minimum candidate count to trigger the advanced rerank strategy.
    #[serde(default = "default_rerank_threshold")]
    pub rerank_threshold: usize,
    /// Advanced rerank strategy. Valid: "none", "mmr".
    #[serde(default = "default_rerank_strategy")]
    pub rerank_strategy: String,
    /// MMR relevance-vs-diversity weight, where 1.0 means relevance-only.
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f64,
    /// Importance weight used by the recall blend.
    #[serde(default = "default_importance_weight")]
    pub importance_weight: f64,
    /// Recency weight used by the recall blend.
    #[serde(default = "default_recency_weight")]
    pub recency_weight: f64,
    /// Reserved (0.0-1.0): the FTS score above which recall would skip the
    /// vector stage. Inert until the backend exposes distinct FTS and vector
    /// operations; recall is a single hybrid call today, so this has no effect.
    #[serde(default = "default_fts_early_return_score")]
    pub fts_early_return_score: f64,

    // ── Namespace Isolation ─────────────────────────────────────
    /// Default namespace for memory entries.
    #[serde(default = "default_namespace")]
    pub default_namespace: String,

    // ── Conflict Resolution ─────────────────────────────────────
    /// Cosine similarity threshold for conflict detection (0.0–1.0).
    #[serde(default = "default_conflict_threshold")]
    pub conflict_threshold: f64,
    /// Enable reversible supersede soft-hide machinery when wired.
    #[serde(default = "default_conflict_supersede_enabled")]
    pub conflict_supersede_enabled: bool,
    /// Enable write-time near-duplicate detection.
    #[serde(default)]
    pub dedup_on_write: bool,
    /// Jaccard threshold for text-only duplicate detection.
    #[serde(default = "default_dedup_jaccard_threshold")]
    pub dedup_jaccard_threshold: f64,
    /// Action to take when a duplicate is detected.
    #[serde(default)]
    pub dedup_action: MemoryDedupAction,

    // ── Memory Budget / Pinning ────────────────────────────────
    /// Maximum Core rows before budget compaction. 0 = unbounded.
    #[serde(default)]
    pub core_max_rows: u64,
    /// Maximum Core bytes before budget compaction. 0 = unbounded.
    #[serde(default)]
    pub core_max_bytes: u64,
    /// Maximum Daily rows before budget compaction. 0 = unbounded.
    #[serde(default)]
    pub daily_max_rows: u64,
    /// Eviction ordering for budget compaction.
    #[serde(default)]
    pub evict_order: MemoryEvictOrder,
    /// Namespaces protected from budget eviction.
    #[serde(default)]
    pub pin_namespaces: Vec<String>,
    /// Pin entries at or above this importance. >1.0 means disabled.
    #[serde(default = "default_pin_min_importance")]
    pub pin_min_importance: f64,

    // ── Audit Trail ─────────────────────────────────────────────
    /// Enable audit logging of memory operations.
    #[serde(default)]
    pub audit_enabled: bool,
    /// Retention period for audit entries in days (default: 30).
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u32,

    // ── Policy Engine ───────────────────────────────────────────
    /// Memory policy configuration.
    #[serde(default)]
    #[nested]
    pub policy: MemoryPolicyConfig,

    /// Typed memory configuration (`[memory.types]` section).
    #[serde(default)]
    #[nested]
    pub types: MemoryTypesConfig,
    // Backend-specific config fields (e.g. markdown directory) live on
    // `[storage.<backend>.<alias>]`. When a storage alias is configured,
    // runtime looks up the typed config via `Config::resolve_active_storage`.
}

/// Typed memory configuration (`[memory.types]` section).
///
/// Behaviour-neutral by default: `enabled` gates MemoryKind assignment on new
/// consolidation writes and defaults off; the flip is sequenced in a later
/// phase. MemoryCore-only: enabling requires the memorycore memory backend globally
/// and on every agent (validated at config load).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "memory.types"]
pub struct MemoryTypesConfig {
    /// Assign a first-class MemoryKind to new consolidation writes.
    #[serde(default)]
    pub enabled: bool,
}

/// Memory policy configuration (`[memory.policy]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "memory.policy"]
pub struct MemoryPolicyConfig {
    /// Maximum entries per namespace (0 = unlimited).
    #[serde(default)]
    pub max_entries_per_namespace: usize,
    /// Maximum entries per category (0 = unlimited).
    #[serde(default)]
    pub max_entries_per_category: usize,
    /// Retention days by category (overrides global). Keys: "core", "daily", "conversation".
    #[serde(default)]
    pub retention_days_by_category: std::collections::HashMap<String, u32>,
    /// Namespaces that are read-only (writes are rejected).
    #[serde(default)]
    pub read_only_namespaces: Vec<String>,
    /// Content scan mode for durable memory writes: "off", "on", or "strict".
    #[serde(default = "default_memory_threat_scan")]
    pub threat_scan: String,
    /// Re-scan stored entries at recall/read time and withhold flagged entries.
    #[serde(default = "default_true")]
    pub threat_scan_load_time: bool,
    /// Behavior when a write-time content scan matches: "reject" or
    /// "block-on-read".
    #[serde(default = "default_memory_threat_scan_on_hit")]
    pub threat_scan_on_hit: String,
    /// Redact configured secret/PII categories before persistence.
    #[serde(default)]
    pub redact_on_write: bool,
    /// Redaction categories applied when `redact_on_write` is true.
    #[serde(default = "default_memory_redact_categories")]
    pub redact_categories: Vec<String>,
}

impl Default for MemoryPolicyConfig {
    fn default() -> Self {
        Self {
            max_entries_per_namespace: 0,
            max_entries_per_category: 0,
            retention_days_by_category: std::collections::HashMap::new(),
            read_only_namespaces: Vec::new(),
            threat_scan: default_memory_threat_scan(),
            threat_scan_load_time: true,
            threat_scan_on_hit: default_memory_threat_scan_on_hit(),
            redact_on_write: false,
            redact_categories: default_memory_redact_categories(),
        }
    }
}

fn default_memory_threat_scan() -> String {
    "on".into()
}

fn default_memory_threat_scan_on_hit() -> String {
    "reject".into()
}

fn default_memory_redact_categories() -> Vec<String> {
    ["secret", "api_key", "private_key", "email", "phone"]
        .into_iter()
        .map(String::from)
        .collect()
}

pub(crate) fn default_retrieval_stages() -> Vec<String> {
    // No "cache": the hot cache is opt-in so activating the retrieval decorator
    // does not change default per-agent recall. "fts"/"vector" are reserved and
    // inert (recall is a single hybrid backend call today).
    vec!["fts".into(), "vector".into()]
}
pub(crate) fn default_rerank_threshold() -> usize {
    5
}

/// Largest allowed multiplier for the bounded query-time rerank candidate pool.
pub const MAX_MEMORY_RERANK_CANDIDATE_MULTIPLIER: usize = 20;

fn default_candidate_multiplier() -> usize {
    4
}
fn default_rerank_strategy() -> String {
    "none".into()
}
fn default_mmr_lambda() -> f64 {
    0.7
}
fn default_importance_weight() -> f64 {
    0.2
}
fn default_recency_weight() -> f64 {
    0.1
}
pub(crate) fn default_fts_early_return_score() -> f64 {
    0.85
}

fn default_namespace() -> String {
    "default".into()
}
fn default_conflict_threshold() -> f64 {
    0.85
}
fn default_conflict_supersede_enabled() -> bool {
    true
}
fn default_dedup_jaccard_threshold() -> f64 {
    0.80
}
fn default_pin_min_importance() -> f64 {
    1.01
}

/// Write-time duplicate handling policy for memory entries.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MemoryDedupAction {
    /// Reject an incoming near-duplicate.
    #[default]
    Reject,
    /// Merge an incoming near-duplicate into a survivor.
    Merge,
}

/// Memory budget eviction order.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MemoryEvictOrder {
    /// Evict the lowest-value rows first.
    #[default]
    Value,
    /// Evict the oldest rows first.
    Oldest,
}

fn default_audit_retention_days() -> u32 {
    30
}

fn default_embedding_provider() -> String {
    "none".into()
}
fn default_auto_save() -> bool {
    true
}
fn default_hygiene_enabled() -> bool {
    true
}
fn default_archive_after_days() -> u32 {
    7
}
fn default_purge_after_days() -> u32 {
    30
}
fn default_conversation_retention_days() -> u32 {
    30
}
fn default_zero_retention() -> u32 {
    0
}
fn default_embedding_model() -> String {
    "text-embedding-3-small".into()
}
fn default_embedding_dims() -> usize {
    1536
}
fn default_vector_weight() -> f64 {
    0.7
}
fn default_keyword_weight() -> f64 {
    0.3
}
fn default_min_relevance_score() -> f64 {
    0.4
}
fn default_cache_size() -> usize {
    10_000
}
fn default_chunk_size() -> usize {
    512
}
fn default_response_cache_ttl() -> u32 {
    60
}
fn default_response_cache_max() -> usize {
    5_000
}

fn default_response_cache_hot_entries() -> usize {
    256
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: "memorycore".into(),
            auto_save: true,
            hygiene_enabled: default_hygiene_enabled(),
            consolidation_extract_facts: false,
            archive_after_days: default_archive_after_days(),
            purge_after_days: default_purge_after_days(),
            conversation_retention_days: default_conversation_retention_days(),
            daily_retention_days: default_zero_retention(),
            core_retention_days: default_zero_retention(),
            embedding_provider: default_embedding_provider(),
            embedding_model: default_embedding_model(),
            embedding_dimensions: default_embedding_dims(),
            auto_reindex_on_identity_change: false,
            embedding_api_key: None,
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            search_mode: SearchMode::default(),
            min_relevance_score: default_min_relevance_score(),
            embedding_cache_size: default_cache_size(),
            chunk_max_tokens: default_chunk_size(),
            response_cache_enabled: false,
            response_cache_ttl_minutes: default_response_cache_ttl(),
            response_cache_max_entries: default_response_cache_max(),
            response_cache_hot_entries: default_response_cache_hot_entries(),

            retrieval_stages: default_retrieval_stages(),
            candidate_multiplier: default_candidate_multiplier(),
            rerank_enabled: false,
            rerank_threshold: default_rerank_threshold(),
            rerank_strategy: default_rerank_strategy(),
            mmr_lambda: default_mmr_lambda(),
            importance_weight: default_importance_weight(),
            recency_weight: default_recency_weight(),
            fts_early_return_score: default_fts_early_return_score(),
            default_namespace: default_namespace(),
            conflict_threshold: default_conflict_threshold(),
            conflict_supersede_enabled: default_conflict_supersede_enabled(),
            dedup_on_write: false,
            dedup_jaccard_threshold: default_dedup_jaccard_threshold(),
            dedup_action: MemoryDedupAction::default(),
            core_max_rows: 0,
            core_max_bytes: 0,
            daily_max_rows: 0,
            evict_order: MemoryEvictOrder::default(),
            pin_namespaces: Vec::new(),
            pin_min_importance: default_pin_min_importance(),
            audit_enabled: false,
            audit_retention_days: default_audit_retention_days(),
            policy: MemoryPolicyConfig::default(),
            types: MemoryTypesConfig::default(),
        }
    }
}

// ── Observability ─────────────────────────────────────────────────

/// Observability sink backend.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ObservabilityBackend {
    #[default]
    #[serde(alias = "noop")]
    None,
    Log,
    Verbose,
}

impl ObservabilityBackend {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Log => "log",
            Self::Verbose => "verbose",
        }
    }
}

/// JSONL log persistence mode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LogPersistence {
    None,
    #[default]
    Rolling,
    Full,
    /// Persist all events, but with ZeroClaw-managed archive rotation: the
    /// active file is rotated to a timestamped archive on a size and/or daily
    /// boundary, and old archives are pruned by count and/or age. Unlike
    /// `rolling` (entry-count trim of the active file), rotated events are
    /// preserved in archive files for later diagnostics. Rotation is tuned via
    /// the `log_persistence_max_bytes`, `log_persistence_rotate_daily`,
    /// `log_persistence_retention_max_files`, and
    /// `log_persistence_retention_max_age_days` settings.
    Rotating,
}

impl LogPersistence {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Rolling => "rolling",
            Self::Full => "full",
            Self::Rotating => "rotating",
        }
    }
}

/// Tool I/O capture policy.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LogToolIo {
    Off,
    #[default]
    Redacted,
    Full,
}

impl LogToolIo {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Redacted => "redacted",
            Self::Full => "full",
        }
    }
}

/// LLM request payload capture policy. Mirrors [`LogToolIo`] but gates the
/// prompt/messages sent on each `llm_request`. Defaults to `Off` because the
/// full payload is the entire system prompt plus conversation every turn.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LogLlmRequestPayload {
    #[default]
    Off,
    Redacted,
    Full,
}

impl LogLlmRequestPayload {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Redacted => "redacted",
            Self::Full => "full",
        }
    }
}

/// Observability backend configuration (`[observability]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "observability"]
pub struct ObservabilityConfig {
    /// Observability sink: none | log | verbose.
    #[serde(default, deserialize_with = "deserialize_enum_lenient")]
    pub backend: ObservabilityBackend,

    /// Log persistence mode: "none" | "rolling" | "full".
    /// Controls whether every event passing through `reiclaw_log::record!`
    /// is appended to the on-disk JSONL log.
    #[serde(
        default = "default_log_persistence",
        alias = "runtime_trace_mode",
        deserialize_with = "deserialize_enum_lenient"
    )]
    pub log_persistence: LogPersistence,

    /// Log persistence file path. Relative paths resolve under workspace_dir.
    #[serde(default = "default_log_persistence_path", alias = "runtime_trace_path")]
    pub log_persistence_path: String,

    /// Maximum entries retained when `log_persistence = "rolling"`.
    #[serde(
        default = "default_log_persistence_max_entries",
        alias = "runtime_trace_max_entries"
    )]
    pub log_persistence_max_entries: usize,

    /// Size threshold in bytes that triggers an archive rotation when
    /// `log_persistence = "rotating"`. The active file is rotated once an
    /// append leaves it at or above this size. `0` disables size-based
    /// rotation. Ignored unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_max_bytes")]
    pub log_persistence_max_bytes: u64,

    /// Rotate the active file to an archive on a UTC day boundary when
    /// `log_persistence = "rotating"`: before the first event of a new day is
    /// appended, a file whose last write fell on an earlier day is archived.
    /// Ignored unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_rotate_daily")]
    pub log_persistence_rotate_daily: bool,

    /// Retention cap on the number of rotated archive files kept alongside the
    /// active file when `log_persistence = "rotating"`. After a rotation the
    /// oldest archives beyond this count are deleted. `0` keeps all archives.
    /// Ignored unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_retention_max_files")]
    pub log_persistence_retention_max_files: usize,

    /// Retention cap on the age (in days) of rotated archive files when
    /// `log_persistence = "rotating"`. After a rotation, archives older than
    /// this many days are deleted. `0` disables age-based cleanup. Ignored
    /// unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_retention_max_age_days")]
    pub log_persistence_retention_max_age_days: u64,

    /// Tool I/O capture policy: "off" | "redacted" | "full".
    /// - `off`: only tool name + outcome + duration land in the log.
    /// - `redacted` (default): tool input + output are leak-scanned and
    ///   truncated at `log_tool_io_truncate_bytes` before persisting.
    /// - `full`: full input + output, still leak-scanned. For operators
    ///   who need replay fidelity and accept the disk cost.
    #[serde(
        default = "default_log_tool_io",
        deserialize_with = "deserialize_enum_lenient"
    )]
    pub log_tool_io: LogToolIo,

    /// Truncate the captured tool input and output at this many bytes when
    /// `log_tool_io = "redacted"`. Truncated events carry an explicit
    /// `tool_output_truncated: true` flag plus `tool_output_original_bytes`.
    #[serde(default = "default_log_tool_io_truncate_bytes")]
    pub log_tool_io_truncate_bytes: usize,

    /// Tool names whose I/O is never logged beyond name + outcome + duration
    /// regardless of `log_tool_io`. Use for tools whose I/O is intrinsically
    /// sensitive (e.g. memory recall against personal namespaces, agent
    /// secret reads). Empty by default.
    #[serde(default)]
    pub log_tool_io_denylist: Vec<String>,

    /// LLM request payload capture: "off" | "redacted" | "full".
    /// - `off` (default): only `messages_count` lands on the `llm_request`
    ///   event. No prompt or conversation content is persisted.
    /// - `redacted`: the full message history (role + content) is leak-scanned
    ///   and truncated at `log_tool_io_truncate_bytes` before persisting.
    /// - `full`: full message history, still leak-scanned, untruncated. For
    ///   operators who need replay fidelity and accept the disk cost.
    ///
    /// Opt-in (default off) because `full`/`redacted` capture the entire system
    /// prompt and conversation on every turn.
    #[serde(
        default = "default_log_llm_request_payload",
        deserialize_with = "deserialize_enum_lenient"
    )]
    pub log_llm_request_payload: LogLlmRequestPayload,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            backend: ObservabilityBackend::None,
            log_persistence: default_log_persistence(),
            log_persistence_path: default_log_persistence_path(),
            log_persistence_max_entries: default_log_persistence_max_entries(),
            log_persistence_max_bytes: default_log_persistence_max_bytes(),
            log_persistence_rotate_daily: default_log_persistence_rotate_daily(),
            log_persistence_retention_max_files: default_log_persistence_retention_max_files(),
            log_persistence_retention_max_age_days: default_log_persistence_retention_max_age_days(
            ),
            log_tool_io: default_log_tool_io(),
            log_tool_io_truncate_bytes: default_log_tool_io_truncate_bytes(),
            log_tool_io_denylist: Vec::new(),
            log_llm_request_payload: default_log_llm_request_payload(),
        }
    }
}

fn default_log_persistence() -> LogPersistence {
    LogPersistence::Rolling
}

fn default_log_persistence_path() -> String {
    "state/runtime-trace.jsonl".to_string()
}

fn default_log_persistence_max_entries() -> usize {
    200
}

/// Size rotation off by default; operators opt in with an explicit byte budget.
fn default_log_persistence_max_bytes() -> u64 {
    0
}

/// Daily rotation is on by default so `rotating` mode produces day-scoped
/// archives out of the box even without a configured size budget.
fn default_log_persistence_rotate_daily() -> bool {
    true
}

/// Keep a week of rotated archives by default.
fn default_log_persistence_retention_max_files() -> usize {
    7
}

/// Age-based cleanup off by default; count-based retention governs unless set.
fn default_log_persistence_retention_max_age_days() -> u64 {
    0
}

fn default_log_tool_io() -> LogToolIo {
    LogToolIo::Redacted
}

fn default_log_llm_request_payload() -> LogLlmRequestPayload {
    LogLlmRequestPayload::Off
}

fn default_log_tool_io_truncate_bytes() -> usize {
    40960
}

// ── Hooks ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "hooks"]
pub struct HooksConfig {
    /// Enable lifecycle hook execution.
    ///
    /// Hooks run in-process with the same privileges as the main runtime.
    /// Keep enabled hook handlers narrowly scoped and auditable.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    #[nested]
    pub builtin: BuiltinHooksConfig,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            builtin: BuiltinHooksConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "hooks.builtin"]
pub struct BuiltinHooksConfig {
    /// Enable the command-logger hook (logs tool calls for auditing).
    #[serde(default)]
    pub command_logger: bool,
    /// Configuration for the webhook-audit hook.
    ///
    /// When enabled, POSTs a JSON payload to `url` for every tool invocation
    /// that matches one of `tool_patterns`.
    #[serde(default)]
    #[nested]
    pub webhook_audit: WebhookAuditConfig,
}

/// Configuration for the webhook-audit builtin hook.
///
/// Sends an HTTP POST with a JSON body to an external endpoint each time
/// a tool call matches one of the configured patterns. Useful for
/// centralised audit logging, SIEM ingestion, or compliance pipelines.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "hooks.builtin.webhook_audit"]
pub struct WebhookAuditConfig {
    /// Enable the webhook-audit hook. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Target URL that will receive the audit POST requests.
    #[serde(default)]
    pub url: String,
    /// Glob patterns for tool names to audit (e.g. `["Bash", "Write"]`).
    /// An empty list means **no** tools are audited.
    #[serde(default)]
    pub tool_patterns: Vec<String>,
    /// Include tool call arguments in the audit payload. Default: `false`.
    ///
    /// Be mindful of sensitive data — arguments may contain secrets or PII.
    #[serde(default)]
    pub include_args: bool,
    /// Maximum size (in bytes) of serialised arguments included in a single
    /// audit payload. Arguments exceeding this limit are truncated.
    /// Default: `4096`.
    #[serde(default = "default_max_args_bytes")]
    pub max_args_bytes: u64,
}

fn default_max_args_bytes() -> u64 {
    4096
}

impl Default for WebhookAuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            tool_patterns: Vec::new(),
            include_args: false,
            max_args_bytes: default_max_args_bytes(),
        }
    }
}

// ── Autonomy / Security ──────────────────────────────────────────
//
// All policy fields live on per-agent `[risk_profiles.<alias>]` entries
// (see `RiskProfileConfig` below). `Config::active_risk_profile(agent_alias)`
// resolves the active profile for any callsite (agent-driven or non-agent
// contexts). A missing alias is not synthesized during loading.

pub fn default_auto_approve() -> Vec<String> {
    vec![
        "file_read".into(),
        "memory_recall".into(),
        "web_search_tool".into(),
        "web_fetch".into(),
        "glob_search".into(),
        "content_search".into(),
        "tool_search".into(),
    ]
}

fn default_always_ask() -> Vec<String> {
    vec![]
}

impl RiskProfileConfig {
    /// Merge the built-in default `auto_approve` entries into the current
    /// list, preserving any user-supplied additions.
    pub fn ensure_default_auto_approve(&mut self) {
        let defaults = default_auto_approve();
        for entry in defaults {
            if !self.auto_approve.iter().any(|existing| existing == &entry) {
                self.auto_approve.push(entry);
            }
        }
    }

    /// Synthesize a [`SandboxConfig`] from this profile's flattened sandbox
    /// fields. Sandbox config is stored flat on the profile; callsites that
    /// still want a `SandboxConfig` instance (sandbox detection in
    /// `zeroclaw-runtime::security::detect`) can call this helper.
    #[must_use]
    pub fn sandbox_config(&self) -> SandboxConfig {
        let backend = self
            .sandbox_backend
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(parse_sandbox_backend)
            .unwrap_or_default();
        SandboxConfig {
            enabled: self.sandbox_enabled,
            backend,
            firejail_args: self.firejail_args.clone(),
        }
    }
}

fn parse_sandbox_backend(name: &str) -> SandboxBackend {
    match name.to_ascii_lowercase().as_str() {
        "auto" => SandboxBackend::Auto,
        "landlock" => SandboxBackend::Landlock,
        "firejail" => SandboxBackend::Firejail,
        "bubblewrap" => SandboxBackend::Bubblewrap,
        "docker" => SandboxBackend::Docker,
        "sandbox-exec" | "sandboxexec" | "seatbelt" => SandboxBackend::SandboxExec,
        "none" => SandboxBackend::None,
        _ => SandboxBackend::default(),
    }
}

// ── Profiles & Bundles ───────────────────────────────────────────

/// Named risk/autonomy profile (`[risk_profiles.<alias>]`).
///
/// Unified policy surface. Agents reference a profile by alias and the
/// runtime resolves through it for shell command allowlists, approval gates,
/// sandbox/resource limits, and delegation guardrails. The conventional
/// `risk_profiles["default"]` is the resolution target for non-agent
/// contexts (orchestrator init, cron worker startup); the `Default` impl
/// below mirrors the legacy safety-first defaults so a fresh install
/// behaves the same as a config from before the per-profile split.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "risk_profile"]
#[serde(default)]
pub struct RiskProfileConfig {
    /// Autonomy level applied to this profile. Default: `supervised`.
    pub level: AutonomyLevel,
    /// Restrict filesystem access to workspace-relative paths. Default: `false`.
    pub workspace_only: bool,
    /// Allowlist of executable names for shell execution.
    pub allowed_commands: Vec<String>,
    /// Explicit path denylist.
    pub forbidden_paths: Vec<String>,
    /// Require approval for medium-risk operations.
    pub require_approval_for_medium_risk: bool,
    /// Block high-risk commands even when allowlisted.
    pub block_high_risk_commands: bool,
    /// Environment variable names passed through to shell subprocesses.
    #[credential_class = "legacy_env_path"]
    pub shell_env_passthrough: Vec<String>,
    /// Tools that never require approval in this profile.
    pub auto_approve: Vec<String>,
    /// Tools that always require approval in this profile.
    pub always_ask: Vec<String>,
    /// Extra directory roots the agent may access.
    #[serde(alias = "allowed_path", alias = "allowed_paths")]
    pub allowed_roots: Vec<String>,
    /// Route this profile's tool approvals to a DISTINCT approver channel instead of the
    /// channel that triggered the run (closes the cross-channel-HITL gap). Absent ⇒ the
    /// originating channel approves (today's behavior). See [`crate::autonomy::ApprovalRoute`].
    ///
    /// Honored on both the interactive channel-driven path and the non-interactive turn path
    /// (gateway chat/webhook dispatch and agent-to-agent peer messages). On the non-interactive
    /// path the approver is resolved from the live daemon channel registry; if no registry is
    /// available or the approver is not live, the gate keeps the non-interactive default
    /// (fail-closed deny under the default `on_no_approver`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_route: Option<crate::autonomy::ApprovalRoute>,
    /// Tools the agent may call in agentic mode. `None` = inherit / no
    /// authorization constraint; `Some(vec![])` = deny every tool;
    /// `Some(list)` = allow only the listed names. Authorization decision:
    /// which tools is the agent permitted to invoke at all. See
    /// `excluded_tools` for the inverse denylist scoped to non-CLI channels.
    ///
    /// MCP exception: when the list is non-empty, runtime-discovered MCP
    /// tools (any name containing `__`, which is the `<server>__<tool>`
    /// convention used by the MCP wrapper) are auto-admitted into the
    /// effective allow-list without needing to be listed here individually.
    /// This keeps the post-change eager-MCP default usable for agents with an
    /// explicit allow-list. Block individual MCP tools via `excluded_tools`.
    ///
    /// Scope of the exception: the `__` auto-admit applies only to this
    /// risk-profile allow-list, **not** to caller-supplied per-run
    /// `allowed_tools` (cron job `allowed_tools`, narrowed delegate
    /// invocations, etc.). Per-run lists are still strict explicit-list
    /// intersections, so a job that narrows `allowed_tools = ["cron_add"]`
    /// will not see runtime-discovered MCP tools unless it names them: a
    /// narrowed list is an explicit statement of intent, and silently
    /// widening it would defeat the narrowing.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Tools excluded from non-CLI channels under this profile.
    ///
    /// Also subtracts from the agentic-delegate allow-list resolved at
    /// runtime, which is the only way to block individual
    /// `<server>__<tool>` MCP names that would otherwise be auto-admitted
    /// by the `allowed_tools` MCP exception described above.
    pub excluded_tools: Vec<String>,
    /// Whether a non-empty `allowed_tools` auto-admits runtime-discovered
    /// MCP tools. Defaults to
    /// [`McpDiscoveredToolPolicy::ExplicitOnly`][crate::autonomy::McpDiscoveredToolPolicy::ExplicitOnly]:
    /// an MCP tool must be named like any other. Set `auto_admit` to restore
    /// the permissive behavior described on `allowed_tools` above.
    #[serde(default)]
    pub mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy,
    // ── Sandbox (from security.sandbox) ─────────────────────────────
    /// Whether the sandbox is enabled for this profile. `None` inherits global.
    pub sandbox_enabled: Option<bool>,
    /// Sandbox backend identifier (e.g. `"firejail"`, `"landlock"`). `None` inherits.
    pub sandbox_backend: Option<String>,
    /// Extra arguments forwarded to firejail when sandbox_backend = "firejail".
    pub firejail_args: Vec<String>,
}

impl Default for RiskProfileConfig {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Supervised,
            workspace_only: true,
            allowed_commands: crate::policy_defaults::default_allowed_commands(),
            forbidden_paths: crate::policy_defaults::default_forbidden_paths(),
            require_approval_for_medium_risk: true,
            block_high_risk_commands: true,
            shell_env_passthrough: vec![],
            auto_approve: default_auto_approve(),
            always_ask: default_always_ask(),
            allowed_roots: Vec::new(),
            approval_route: None,
            allowed_tools: None,
            excluded_tools: Vec::new(),
            mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy::default(),
            sandbox_enabled: None,
            sandbox_backend: None,
            firejail_args: Vec::new(),
        }
    }
}

/// Named runtime/LLM execution profile (`[runtime_profiles.<alias>]`).
///
/// Reusable operational tuning: agentic mode, iteration caps, context
/// budget, parallel dispatch, resource ceilings, and the budget knobs
/// that `SecurityPolicy` enforces with subagent parent-subset
/// discipline. (The spawn-depth knob retired with the spawn wall:
/// local recursion is the fixed D1 law, and the coordinator child host
/// that had its own admission cap was deleted with the control-plane
/// migration wall.) Anything authorization-shaped (allowed
/// commands/tools/paths, approval gates, sandbox) lives on
/// `[risk_profiles.<alias>]`. Anything model-provider shaped (model,
/// temperature, max_tokens, timeout_secs) lives on
/// `[providers.models.<type>.<alias>]`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "runtime_profile"]
#[serde(default)]
pub struct RuntimeProfileConfig {
    /// Enable agentic (multi-turn tool-call loop) mode.
    pub agentic: bool,
    /// Maximum tool-call iterations in agentic mode. `0` inherits the global default.
    pub max_tool_iterations: usize,
    // ── Budget caps (enforced with subagent parent-subset discipline) ──
    /// Maximum actions allowed per hour. `0` is a hard zero budget — the
    /// per-sender rate tracker treats a max of 0 as always exhausted
    /// (`PerSenderTracker::is_exhausted`), blocking every action. For an
    /// effectively-unlimited budget use a high value (e.g. `u32::MAX`), not 0.
    /// `SecurityPolicy::ensure_no_escalation_beyond` rejects subagents
    /// that try to raise this above the parent's value.
    pub max_actions_per_hour: u32,
    /// Maximum cost per day in cents. `0` inherits the global limit.
    /// Parent-subset enforced for subagents.
    pub max_cost_per_day_cents: u32,
    /// Shell subprocess timeout in seconds. `0` inherits the global timeout.
    /// Parent-subset enforced for subagents.
    pub shell_timeout_secs: u64,
    // ── Per-agent runtime tunables (also live on AliasedAgentConfig) ─
    /// Maximum conversation history messages retained per session. `None` inherits.
    pub max_history_messages: Option<usize>,
    /// Maximum estimated tokens for context before compaction. `None` inherits.
    pub max_context_tokens: Option<usize>,
    /// Use compact bootstrap (6000 chars / 2 RAG chunks). `None` inherits.
    pub compact_context: Option<bool>,
    /// Enable parallel tool execution per iteration. `None` inherits.
    pub parallel_tools: Option<bool>,
    /// Tool dispatch strategy (e.g. `"auto"`). `None` inherits.
    pub tool_dispatcher: Option<String>,
    /// Tools exempt from within-turn dedup check.
    pub tool_call_dedup_exempt: Vec<String>,
    /// Maximum characters for the assembled system prompt. `None` inherits.
    pub max_system_prompt_chars: Option<usize>,
    /// Maximum characters for a single tool result. `None` inherits.
    pub max_tool_result_chars: Option<usize>,
    /// Number of recent turns whose full tool context is preserved. `None` inherits.
    pub keep_tool_context_turns: Option<usize>,
    /// Maximum memory entries injected per turn. `None` inherits global default (5).
    /// Set to `0` for unlimited.
    pub memory_recall_limit: Option<usize>,
    /// How skills are injected into the system prompt for agents on this
    /// profile. `None` inherits the global `[skills] prompt_injection_mode`;
    /// `compact` inlines only compact skill metadata and registers the
    /// `read_skill` tool, `full` inlines full skill instructions. Resolved
    /// through [`Config::effective_skills_prompt_mode`], the single point both
    /// the system-prompt builder and the `read_skill` tool-registration gate
    /// consult so they always agree on the effective mode.
    pub prompt_injection_mode: Option<SkillsPromptInjectionMode>,
    pub strict_tool_parsing: bool,
    #[nested]
    pub thinking: crate::scattered_types::ThinkingConfig,
    #[nested]
    pub history_pruning: crate::scattered_types::HistoryPrunerConfig,
    #[nested]
    pub eval: crate::scattered_types::EvalConfig,
    #[nested]
    pub auto_classify: Option<crate::scattered_types::AutoClassifyConfig>,
    #[nested]
    pub context_compression: crate::scattered_types::ContextCompressionConfig,
    #[nested]
    pub tool_receipts: ToolReceiptsConfig,
    pub tool_filter_groups: Vec<ToolFilterGroup>,
}

impl Default for RuntimeProfileConfig {
    fn default() -> Self {
        Self {
            agentic: false,
            max_tool_iterations: 0,
            max_actions_per_hour: 20,
            max_cost_per_day_cents: 500,
            shell_timeout_secs: 60,
            max_history_messages: None,
            max_context_tokens: None,
            compact_context: None,
            parallel_tools: None,
            tool_dispatcher: None,
            tool_call_dedup_exempt: Vec::new(),
            max_system_prompt_chars: None,
            max_tool_result_chars: None,
            keep_tool_context_turns: None,
            memory_recall_limit: None,
            prompt_injection_mode: None,
            strict_tool_parsing: false,
            thinking: crate::scattered_types::ThinkingConfig::default(),
            history_pruning: crate::scattered_types::HistoryPrunerConfig::default(),
            eval: crate::scattered_types::EvalConfig::default(),
            auto_classify: None,
            context_compression: crate::scattered_types::ContextCompressionConfig::default(),
            tool_receipts: ToolReceiptsConfig::default(),
            tool_filter_groups: Vec::new(),
        }
    }
}

/// Named skill bundle (`[skill_bundles.<alias>]`).
///
/// A reusable group of skills that can be attached to an agent or channel
/// by alias, controlling which skills are loaded and from where.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "skill_bundle"]
#[serde(default)]
pub struct SkillBundleConfig {
    /// Directory path (relative to workspace root) to load skills from.
    pub directory: Option<String>,
    /// Skill names to include. Empty means include all skills in `directory`.
    pub include: Vec<String>,
    /// Skill names to exclude from this bundle.
    pub exclude: Vec<String>,
}

impl SkillBundleConfig {
    /// Whether a skill name survives this bundle's include/exclude filter.
    /// Empty `include` admits every name; `exclude` always wins.
    pub fn admits_skill(&self, name: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|s| s == name) {
            return false;
        }
        !self.exclude.iter().any(|s| s == name)
    }
}

/// Named MCP server bundle (`[mcp_bundles.<alias>]`).
///
/// A reusable group of MCP servers granted to an agent that references the
/// bundle by alias in `agents.<alias>.mcp_bundles`. Server IDs are matched
/// against `[mcp.servers]` by `name`. Resolution is secure by default (see
/// `Config::mcp_servers_for_bundles`): an ID with no matching server grants
/// nothing, and `exclude` wins over `servers` across every bundle an agent
/// references.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "mcp_bundle"]
#[serde(default)]
pub struct McpBundleConfig {
    /// MCP server IDs (`[mcp.servers].name`) granted by this bundle.
    pub servers: Vec<String>,
    /// MCP server IDs removed from the grant. Deny wins: a name listed here is
    /// excluded even if another referenced bundle includes it.
    pub exclude: Vec<String>,
}

// ── Runtime ──────────────────────────────────────────────────────

/// Runtime adapter kind.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    #[default]
    Native,
    Docker,
    Cloudflare,
}

impl RuntimeKind {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Docker => "docker",
            Self::Cloudflare => "cloudflare",
        }
    }
}

/// Runtime adapter configuration (`[runtime]` section).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "runtime"]
pub struct RuntimeConfig {
    /// Runtime kind: native | docker | cloudflare.
    #[serde(default, deserialize_with = "deserialize_enum_lenient")]
    pub kind: RuntimeKind,

    /// Docker runtime settings (used when `kind = "docker"`).
    #[serde(default)]
    #[nested]
    pub docker: DockerRuntimeConfig,

    /// Shell binary the native runtime uses for command execution.
    ///
    /// Applies only to `runtime.kind = "native"`; other runtimes ignore it.
    /// When unset or `null`, the system default `sh` is used. The shell is
    /// invoked as `<shell> -c "<command>"`, so it must be a POSIX-compatible
    /// shell binary.
    ///
    /// Accepted forms (Unix):
    /// - a bare command name resolved via `PATH` (e.g. `"bash"`), or
    /// - an absolute path (e.g. `"/bin/bash"`, `"/usr/bin/zsh"`).
    ///
    /// The value is validated when the native runtime is constructed, so a bad
    /// value is reported up front rather than failing on the first shell
    /// command. Rejected: empty/whitespace; a relative path with separators
    /// (e.g. `"./sh"`, `"bin/sh"` — use a bare `PATH` name or an absolute path
    /// instead); a bare name not found on `PATH`; and a path that does not
    /// exist or is not executable.
    ///
    /// **Ignored on Android** (and not validated there): Android always uses
    /// `/system/bin/sh` (its shell is not on `PATH` for spawned processes).
    ///
    /// **Examples:**
    /// ```toml
    /// [runtime]
    /// shell = "bash" # resolves via PATH
    /// shell = "/bin/zsh" # absolute path
    /// ```
    #[serde(default)]
    pub shell: Option<String>,

    /// Global reasoning override for model_providers that expose explicit controls.
    /// - `None`: model_provider default behavior
    /// - `Some(true)`: request reasoning/thinking when supported
    /// - `Some(false)`: disable reasoning/thinking when supported
    #[serde(default)]
    pub reasoning_enabled: Option<bool>,
    /// Optional reasoning effort for model_providers that expose a level control.
    #[serde(default, deserialize_with = "deserialize_reasoning_effort_opt")]
    pub reasoning_effort: Option<String>,
}

/// Docker runtime configuration (`[runtime.docker]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "runtime.docker"]
pub struct DockerRuntimeConfig {
    /// Runtime image used to execute shell commands.
    #[serde(default = "default_docker_image")]
    pub image: String,

    /// Docker network mode (`none`, `bridge`, etc.).
    #[serde(default = "default_docker_network")]
    pub network: String,

    /// Optional memory limit in MB (`None` = no explicit limit).
    #[serde(default = "default_docker_memory_limit_mb")]
    pub memory_limit_mb: Option<u64>,

    /// Optional CPU limit (`None` = no explicit limit).
    #[serde(default = "default_docker_cpu_limit")]
    pub cpu_limit: Option<f64>,

    /// Mount root filesystem as read-only.
    #[serde(default = "default_true")]
    pub read_only_rootfs: bool,

    /// Mount configured workspace into `/workspace`.
    ///
    /// When enabled, the workspace must exist and canonicalize before Docker
    /// command construction.
    #[serde(default = "default_true")]
    pub mount_workspace: bool,

    /// Optional workspace root allowlist for fail-closed Docker mount validation: when `mount_workspace` is enabled, the workspace must exist and canonicalize even when this list is empty; every configured root must also exist and canonicalize; one invalid entry rejects the command before Docker starts; an empty list permits any canonical workspace.
    #[serde(default)]
    pub allowed_workspace_roots: Vec<String>,
}

fn default_docker_image() -> String {
    "alpine:3.20".into()
}

fn default_docker_network() -> String {
    "none".into()
}

fn default_docker_memory_limit_mb() -> Option<u64> {
    Some(512)
}

fn default_docker_cpu_limit() -> Option<f64> {
    Some(1.0)
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            image: default_docker_image(),
            network: default_docker_network(),
            memory_limit_mb: default_docker_memory_limit_mb(),
            cpu_limit: default_docker_cpu_limit(),
            read_only_rootfs: true,
            mount_workspace: true,
            allowed_workspace_roots: Vec::new(),
        }
    }
}

// ── Reliability / supervision ────────────────────────────────────

/// Reliability and supervision configuration (`[reliability]` section).
///
/// Controls model_provider retries, API key rotation, and channel restart backoff.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "reliability"]
pub struct ReliabilityConfig {
    /// Retries per model_provider before bailing.
    #[serde(default = "default_provider_retries")]
    pub provider_retries: u32,
    /// Base backoff (ms) for model_provider retry delay.
    #[serde(default = "default_provider_backoff_ms")]
    pub provider_backoff_ms: u64,
    /// Additional API keys for round-robin rotation on rate-limit (429) errors.
    /// The primary `api_key` is always tried first; these are extras.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_keys: Vec<String>,
    /// Initial backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_secs")]
    pub channel_initial_backoff_secs: u64,
    /// Max backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_max_secs")]
    pub channel_max_backoff_secs: u64,
    /// Scheduler polling cadence in seconds.
    #[serde(default = "default_scheduler_poll_secs")]
    pub scheduler_poll_secs: u64,
    /// Max retries for cron job execution attempts.
    #[serde(default = "default_scheduler_retries")]
    pub scheduler_retries: u32,
}

fn default_provider_retries() -> u32 {
    2
}

fn default_provider_backoff_ms() -> u64 {
    500
}

fn default_channel_backoff_secs() -> u64 {
    2
}

fn default_channel_backoff_max_secs() -> u64 {
    60
}

fn default_scheduler_poll_secs() -> u64 {
    15
}

fn default_scheduler_retries() -> u32 {
    2
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            provider_retries: default_provider_retries(),
            provider_backoff_ms: default_provider_backoff_ms(),
            api_keys: Vec::new(),
            channel_initial_backoff_secs: default_channel_backoff_secs(),
            channel_max_backoff_secs: default_channel_backoff_max_secs(),
            scheduler_poll_secs: default_scheduler_poll_secs(),
            scheduler_retries: default_scheduler_retries(),
        }
    }
}

// ── Scheduler ────────────────────────────────────────────────────

/// Scheduler configuration for periodic task execution (`[scheduler]` section).
///
/// Owns the cron-runtime knobs: per-job declarations live on
/// `Config.cron: HashMap<String, CronJobDecl>` (alias-keyed), while the
/// scheduler loop's runtime behavior (`enabled`, polling cap, catch-up) lives here.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "scheduler"]
pub struct SchedulerConfig {
    /// Enable the built-in scheduler loop. When false, no cron jobs run.
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
    /// Maximum number of persisted scheduled tasks per polling cycle.
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    /// Maximum tasks executed in parallel within a single polling cycle.
    #[serde(default = "default_scheduler_max_concurrent")]
    pub max_concurrent: usize,
    /// Run all overdue jobs at scheduler startup. Default: `true`.
    ///
    /// When the daemon restarts late, jobs whose `next_run` is in the past
    /// fire once before normal polling resumes. Disable to wait for the
    /// next scheduled occurrence instead.
    #[serde(default = "default_true")]
    pub catch_up_on_startup: bool,
    /// Maximum number of historical cron run records to retain. Default: `50`.
    #[serde(default = "default_max_run_history")]
    pub max_run_history: u32,
}

fn default_scheduler_enabled() -> bool {
    true
}

fn default_scheduler_max_tasks() -> usize {
    64
}

fn default_scheduler_max_concurrent() -> usize {
    4
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_scheduler_enabled(),
            max_tasks: default_scheduler_max_tasks(),
            max_concurrent: default_scheduler_max_concurrent(),
            catch_up_on_startup: true,
            max_run_history: default_max_run_history(),
        }
    }
}

// ── Model routing ────────────────────────────────────────────────

/// Route a task hint to a specific model_provider + model.
///
/// ```toml
/// [[model_routes]]
/// hint = "reasoning"
/// model_provider = "openai.default"
/// model = "gpt-5.2"
///
/// [[model_routes]]
/// hint = "fast"
/// model_provider = "groq.low-latency"
/// model = "llama-3.3-70b-versatile"
/// ```
///
/// Usage: pass `hint:reasoning` as the model parameter to route the request.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "model_routes"]
pub struct ModelRouteConfig {
    /// Task hint name (e.g. "reasoning", "fast", "code", "summarize")
    /// `#[serde(default)]` lets the `create_map_key` macro default-construct
    /// from `{}` before the hint gets injected via `set_prop`. Empty strings
    /// are rejected by `Config::validate()`.
    #[serde(default)]
    pub hint: String,
    /// Dotted provider profile ref to route to (must resolve to `providers.models.<type>.<alias>`)
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model_provider: String,
    /// Provider-local model identifier to use with that provider profile
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model: String,
    /// Optional API key override for this route's model provider
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
}

// ── Embedding routing ───────────────────────────────────────────

/// Route an embedding hint to a specific model_provider + model.
///
/// ```toml
/// [[embedding_routes]]
/// hint = "semantic"
/// model_provider = "openai.embeddings"
/// model = "text-embedding-3-small"
/// dimensions = 1536
///
/// [memory]
/// embedding_model = "hint:semantic"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "embedding_routes"]
pub struct EmbeddingRouteConfig {
    /// Route hint name (e.g. "semantic", "archive", "faq")
    /// `#[serde(default)]` lets the `create_map_key` macro default-construct
    /// from `{}` before the hint gets injected via `set_prop`. Empty strings
    /// are rejected by `Config::validate()`.
    #[serde(default)]
    pub hint: String,
    /// Dotted embedding-capable provider profile ref
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model_provider: String,
    /// Provider-local embedding model identifier to use with that provider profile
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model: String,
    /// Optional embedding dimension override for this route
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// Optional API key override for this route's model_provider
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
}

// ── Query Classification ─────────────────────────────────────────

/// Automatic query classification — classifies user messages by keyword/pattern
/// and routes to the appropriate model hint. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "query_classification"]
pub struct QueryClassificationConfig {
    /// Enable automatic query classification. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Classification rules evaluated in priority order.
    #[serde(default)]
    pub rules: Vec<ClassificationRule>,
}

/// A single classification rule mapping message patterns to a model hint.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ClassificationRule {
    /// Must match a `[[model_routes]]` hint value.
    pub hint: String,
    /// Case-insensitive substring matches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Case-sensitive literal matches (for "```", "fn ", etc.).
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Only match if message length >= N chars.
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Only match if message length <= N chars.
    #[serde(default)]
    pub max_length: Option<usize>,
    /// Higher priority rules are checked first.
    #[serde(default)]
    pub priority: i32,
}

// ── Heartbeat ────────────────────────────────────────────────────

/// Heartbeat configuration for periodic health pings (`[heartbeat]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "heartbeat"]
#[allow(clippy::struct_excessive_bools)]
pub struct HeartbeatConfig {
    /// Enable periodic heartbeat pings. Default: `false`. When enabled,
    /// `agent` must name a configured agent — there is no default agent
    /// for heartbeat to fall through to.
    #[serde(default)]
    pub enabled: bool,
    /// Configured agent alias the heartbeat worker runs as. Required
    /// when `enabled = true`; refers to a `[agents.<alias>]` entry.
    #[serde(default)]
    pub agent: String,
    /// Interval in minutes between heartbeat pings. Minimum: `1`. Default: `30`.
    #[serde(default = "default_heartbeat_interval")]
    pub interval_minutes: u32,
    /// Enable two-phase heartbeat: Phase 1 asks LLM whether to run, Phase 2
    /// executes only when the LLM decides there is work to do. Saves API cost
    /// during quiet periods. Default: `true`.
    #[serde(default = "default_two_phase")]
    pub two_phase: bool,
    /// Optional fallback task text when `HEARTBEAT.md` has no task entries.
    #[serde(default)]
    pub message: Option<String>,
    /// Optional `[gateway.bridges.<name>]` destination for heartbeat output.
    /// Must allow `/ws/bridge` delivery (no `device` or `owner` scope).
    /// When omitted together with `to`, tasks run without external delivery.
    #[serde(default, alias = "channel")]
    pub target: Option<String>,
    /// Optional delivery recipient/chat identifier (required when `target` is
    /// explicitly set).
    #[serde(default, alias = "recipient")]
    pub to: Option<String>,
    /// Enable adaptive intervals that back off on failures and speed up for
    /// high-priority tasks. Default: `false`.
    #[serde(default)]
    pub adaptive: bool,
    /// Minimum interval in minutes when adaptive mode is enabled. Default: `5`.
    #[serde(default = "default_heartbeat_min_interval")]
    pub min_interval_minutes: u32,
    /// Maximum interval in minutes when adaptive mode backs off. Default: `120`.
    #[serde(default = "default_heartbeat_max_interval")]
    pub max_interval_minutes: u32,
    /// Dead-man's switch timeout in minutes. If the heartbeat has not ticked
    /// within this window, an alert is sent. `0` disables. Default: `0`.
    #[serde(default)]
    pub deadman_timeout_minutes: u32,
    /// Delivery-capable `[gateway.bridges.<name>]` for dead-man's switch
    /// alerts. Falls back to the heartbeat delivery bridge.
    #[serde(default)]
    pub deadman_channel: Option<String>,
    /// Recipient for dead-man's switch alerts. Falls back to `to`.
    #[serde(default)]
    pub deadman_to: Option<String>,
    /// Maximum number of heartbeat run history records to retain. Default: `100`.
    #[serde(default = "default_heartbeat_max_run_history")]
    pub max_run_history: u32,
    /// Load the channel session history before each heartbeat task execution so
    /// the LLM has conversational context. Default: `false`.
    ///
    /// When `true`, the session file for the configured `target`/`to` is passed
    /// to the agent as `session_state_file`, giving it access to the recent
    /// conversation history — just as if the user had sent a message.
    #[serde(default)]
    pub load_session_context: bool,
    /// Maximum wall-clock seconds allowed for a single agent invocation
    /// (Phase 1 decision or Phase 2 task execution). `0` disables.
    /// Default: `600` (10 minutes).
    #[serde(default = "default_heartbeat_task_timeout")]
    pub task_timeout_secs: u64,
}

fn default_heartbeat_interval() -> u32 {
    30
}

fn default_two_phase() -> bool {
    true
}

fn default_heartbeat_min_interval() -> u32 {
    5
}

fn default_heartbeat_max_interval() -> u32 {
    120
}

fn default_heartbeat_max_run_history() -> u32 {
    100
}

fn default_heartbeat_task_timeout() -> u64 {
    600
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agent: String::new(),
            interval_minutes: default_heartbeat_interval(),
            two_phase: true,
            message: None,
            target: None,
            to: None,
            adaptive: false,
            min_interval_minutes: default_heartbeat_min_interval(),
            max_interval_minutes: default_heartbeat_max_interval(),
            deadman_timeout_minutes: 0,
            deadman_channel: None,
            deadman_to: None,
            max_run_history: default_heartbeat_max_run_history(),
            load_session_context: false,
            task_timeout_secs: default_heartbeat_task_timeout(),
        }
    }
}

// ── Cron ────────────────────────────────────────────────────────

/// A declarative cron job definition (`[cron.<alias>]`).
///
/// Stored alias-keyed on `Config.cron`. The map key serves as the stable job id.
/// Synced into the database at scheduler startup with `source = "declarative"`,
/// distinguishing them from jobs created imperatively via CLI or API.
/// Declarative config takes precedence on each sync: if the config changes,
/// the DB is updated to match. Imperative jobs are never deleted by sync.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cron"]
pub struct CronJobDecl {
    /// Human-readable name.
    #[serde(default)]
    pub name: Option<String>,
    /// Job type: `"shell"` (default) or `"agent"`.
    #[serde(default = "default_job_type_decl")]
    pub job_type: String,
    /// Schedule for the job.
    #[serde(default)]
    pub schedule: CronScheduleDecl,
    /// Shell command to run (required when `job_type = "shell"`).
    #[serde(default)]
    pub command: Option<String>,
    /// Agent prompt (required when `job_type = "agent"`).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Whether the job is enabled. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Model override for agent jobs.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional allowlist of tool names for agent jobs. When omitted, scheduler
    /// defaults may still exclude scheduler mutation tools for cron agent jobs.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Whether to recall and inject memory context before this agent job runs.
    /// Defaults to `true`; set to `false` for stateless digest jobs.
    #[serde(default = "default_true")]
    pub uses_memory: bool,
    /// Session target: `"isolated"` (default) or `"main"`.
    #[serde(default)]
    pub session_target: Option<String>,
    /// Delivery configuration.
    #[serde(default)]
    #[nested]
    pub delivery: Option<DeliveryConfigDecl>,
}

impl Default for CronJobDecl {
    fn default() -> Self {
        Self {
            name: None,
            job_type: default_job_type_decl(),
            schedule: CronScheduleDecl::default(),
            command: None,
            prompt: None,
            enabled: true,
            model: None,
            allowed_tools: None,
            uses_memory: true,
            session_target: None,
            delivery: None,
        }
    }
}

/// Schedule variant for declarative cron jobs.
#[derive(Debug, Clone, Serialize, Deserialize, reiclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CronScheduleDecl {
    /// Classic cron expression.
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    /// Interval in milliseconds.
    Every { every_ms: u64 },
    /// One-shot at an RFC 3339 timestamp.
    At { at: String },
}

impl Default for CronScheduleDecl {
    fn default() -> Self {
        // Empty cron expression — `validate_decl` rejects it. Used only as
        // a placeholder when a fresh map entry is auto-created via the
        // schema's `create_map_key` path; the user fills it in immediately.
        Self::Cron {
            expr: String::new(),
            tz: None,
        }
    }
}

/// Delivery configuration for declarative cron jobs.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cron_delivery"]
pub struct DeliveryConfigDecl {
    /// Delivery mode: `"none"` or `"announce"`.
    #[serde(default = "default_delivery_mode")]
    pub mode: String,
    /// Channel name (e.g. `"telegram"`, `"webhook"`).
    #[serde(default)]
    pub channel: Option<String>,
    /// Target/recipient identifier.
    #[serde(default)]
    pub to: Option<String>,
    /// Optional thread/conversation identifier carried into the outbound send.
    /// Required by channels that route on a separate `thread_id` field (e.g.
    /// webhook callbacks bridging into agent-chat platforms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Best-effort delivery. Default: `true`.
    #[serde(default = "default_true")]
    pub best_effort: bool,
}

impl Default for DeliveryConfigDecl {
    fn default() -> Self {
        Self {
            mode: default_delivery_mode(),
            channel: None,
            to: None,
            thread_id: None,
            best_effort: true,
        }
    }
}

fn default_job_type_decl() -> String {
    "shell".to_string()
}

fn default_delivery_mode() -> String {
    "none".to_string()
}

fn default_max_run_history() -> u32 {
    50
}

// ── Channels ─────────────────────────────────────────────────────

/// Channel kinds whose in-process transport runners were retired with the
/// `zeroclaw-channels` crate (#579/#581). Their schemas were removed by
/// #584; leftover sections load with tombstone warnings and are discarded.
/// Keep these historical names at admission boundaries so no surface
/// presents them as available transports.
///
/// The retained channel surfaces are exactly the complement:
/// `telegram` (gateway bridge), `webhook` (gateway ingress) and the always-on
/// CLI. `email` and `discord` were retired by the owner on 2026-11-18 and are
/// listed here so every admission boundary names them as retired; unlike the
/// older entries they are also refused outright at config load
/// (`reject_retired_channel_config`) instead of being tombstoned. The legacy
/// `voice_duplex` event logger is retired under ADR-023; the separate voice
/// surface does not inherit its settings. Do not add a kind here without a
/// consumer-graph review, and do not remove one without the deletion evidence
/// #584 requires.
pub const RETIRED_TRANSPORT_KINDS: &[&str] = &[
    "slack",
    "mattermost",
    "imessage",
    "matrix",
    "signal",
    "whatsapp",
    "irc",
    "twitch",
    "lark",
    "feishu",
    "line",
    "dingtalk",
    "wecom",
    "wecom_ws",
    "wechat",
    "qq",
    "twitter",
    "mochat",
    "nostr",
    "clawdtalk",
    "reddit",
    "bluesky",
    "git",
    "voice_call",
    "voice_wake",
    "voice_duplex",
    "amqp",
    "email",
    "discord",
];

/// Whether `kind` names a retired transport. Accepts the snake_case section
/// key (`voice_wake`) and the kebab-case display kinds `channels()` emits
/// (`voice-wake`, `whatsapp-web`).
pub fn is_retired_transport(kind: &str) -> bool {
    let normalized = kind.replace('-', "_");
    // Preserve legacy WhatsApp Web names at rejection boundaries.
    let canonical = match normalized.as_str() {
        "whatsapp_web" => "whatsapp",
        other => other,
    };
    RETIRED_TRANSPORT_KINDS.contains(&canonical)
}

/// Top-level channel configurations (`[channels]` section).
///
/// each channel type is a keyed table of named instances (aliases).
/// `[channels.telegram.default]` is the conventional single-instance key.
/// Access via `config.channels.telegram.get("default")`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels"]
pub struct ChannelsConfig {
    /// Telegram bot channel instances (`[channels.telegram.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub telegram: HashMap<String, TelegramConfig>,
    /// Webhook channel instances (`[channels.webhook.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub webhook: HashMap<String, WebhookConfig>,
    /// MQTT SOP listener instances — RETIRED. The section now
    /// fails config parse with the retirement message instead of silently
    /// no-op'ing. Field removed with the run-side config sweep.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        deserialize_with = "reject_retired_sop_channel"
    )]
    #[cfg_attr(feature = "schema-export", schemars(skip))]
    pub mqtt: HashMap<String, ()>,
    /// Filesystem SOP listener instances — RETIRED. The section
    /// now fails config parse with the retirement message instead of silently
    /// no-op'ing. Field removed with the run-side config sweep.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        deserialize_with = "reject_retired_sop_channel"
    )]
    #[cfg_attr(feature = "schema-export", schemars(skip))]
    pub filesystem: HashMap<String, ()>,
    /// Whether to send tool-call notification messages (e.g. `🔧 web_search_tool: …`)
    /// to channel users. When `false`, tool calls are still logged server-side but
    /// not forwarded as individual channel messages. Default: `false`.
    #[serde(default = "default_false")]
    pub show_tool_calls: bool,
    /// Persist channel conversation history in SQLite so sessions survive
    /// daemon restarts. Stored in `{workspace}/sessions/sessions.db`. Default: `true`.
    #[serde(default = "default_true")]
    pub session_persistence: bool,
    /// Auto-archive stale sessions older than this many hours. `0` disables. Default: `0`.
    #[serde(default)]
    pub session_ttl_hours: u32,
}

// ── Retired SOP run-side config keys ────────────────────────────────────────
// These deserializers keep the retired keys PARSEABLE only long enough to
// reject them with an actionable message: silently ignoring them would
// reinterpret a run-side config (a SOP dispatch mode, a filesystem/mqtt SOP

fn reject_retired_sop_channel<'de, D, V>(deserializer: D) -> Result<HashMap<String, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    V: Default,
{
    // Any presence of the section — even an empty header — is rejected: the
    // section is retired, presence itself is the misconfiguration.
    let _map = HashMap::<String, serde::de::IgnoredAny>::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "retired SOP run-side channel: MQTT/filesystem listeners were SOP-trigger-only \
         fan-in; SOP runs are Tachi-side ProcedureRuns since #243 (#197 wall 5) — remove \
         this [channels.*] section",
    ))
}

impl ChannelsConfig {
    /// Every configured typed channel entry, preserving the complete alias key.
    /// Presentation metadata and formatted values are not needed by this walk.
    pub fn iter_aliases(&self) -> impl Iterator<Item = (&'static str, &str, bool)> {
        self.telegram
            .iter()
            .map(|(alias, config)| ("telegram", alias.as_str(), config.enabled))
            .chain(
                self.webhook
                    .iter()
                    .map(|(alias, config)| ("webhook", alias.as_str(), config.enabled)),
            )
    }

    /// Returns metadata and configuration status for every known channel type.
    ///
    /// Always returns the full set of channel types regardless of compile-time
    /// feature flags — the `configured` flag reflects whether the operator has
    /// populated that channel's config section, and `retired` marks kinds
    /// whose transport runner no longer exists (`RETIRED_TRANSPORT_KINDS`).
    /// Presentation surfaces must not offer `retired` kinds as connectable
    /// channels.
    pub fn channels(&self) -> Vec<super::traits::ChannelInfo> {
        use super::traits::ChannelInfo;
        vec![
            ChannelInfo {
                kind: "telegram",
                name: "Telegram",
                desc: "connect your bot",
                configured: !self.telegram.is_empty(),
                retired: false,
            },
            ChannelInfo {
                kind: "webhook",
                name: "Webhook",
                desc: "HTTP endpoint",
                configured: !self.webhook.is_empty(),
                retired: false,
            },
        ]
        .into_iter()
        .map(|mut info: super::traits::ChannelInfo| {
            info.retired = is_retired_transport(info.kind);
            info
        })
        .collect()
    }

    /// Returns `true` when at least one channel entry across all channel types
    /// has `enabled = true`. Used by the daemon to decide whether the channels
    /// supervisor should be started — a config with only `enabled = false`
    /// entries (e.g. partially-configured or disabled bots) must not start the
    /// supervisor, otherwise it exits immediately and restarts in a tight loop.
    pub fn has_any_enabled(&self) -> bool {
        self.iter_aliases().any(|(_, _, enabled)| enabled)
    }

    /// One `(canonical_name, configured, deliverable, retired)` row per
    /// channel in the registry. Single source for name-addressed channel
    /// lookups so no surface has to hardcode a subset of the channel list.
    /// `deliverable` distinguishes transports with an outbound delivery path,
    /// so a name-addressed outbound surface
    /// such as `heartbeat.target` refuses them at validation instead of
    /// accepting a target the delivery layer silently drops.
    pub fn channel_presence(&self) -> [(&'static str, bool, bool, bool); 2] {
        [
            ("telegram", !self.telegram.is_empty(), true, false),
            ("webhook", !self.webhook.is_empty(), true, false),
        ]
    }

    /// Whether `name` (case-insensitive) names a channel whose transport
    /// runner is retired (`RETIRED_TRANSPORT_KINDS`). Used by name-addressed
    /// surfaces to give an actionable error instead of a generic
    /// not-deliverable one.
    pub fn is_retired_channel(&self, name: &str) -> bool {
        let needle = name.to_ascii_lowercase();
        is_retired_transport(&needle)
    }

    /// Whether a channel named `name` (case-insensitive) is a known channel
    /// with at least one configured instance. Used by name-addressed surfaces
    /// (e.g. `heartbeat.target`) without hardcoding a subset of the registry.
    pub fn is_channel_configured(&self, name: &str) -> bool {
        let needle = name.to_ascii_lowercase();
        self.channel_presence()
            .iter()
            .any(|(canonical, configured, _, _)| *configured && *canonical == needle)
    }

    /// Whether `name` (case-insensitive) names a channel in the registry,
    /// regardless of whether it is configured.
    pub fn is_known_channel(&self, name: &str) -> bool {
        let needle = name.to_ascii_lowercase();
        self.channel_presence()
            .iter()
            .any(|(canonical, _, _, _)| *canonical == needle)
    }

    /// Whether `name` (case-insensitive) names a channel that can actually
    /// deliver an outbound message. Retired transports are not deliverable:
    /// no delivery path exists for them.
    pub fn is_channel_deliverable(&self, name: &str) -> bool {
        let needle = name.to_ascii_lowercase();
        self.channel_presence()
            .iter()
            .any(|(canonical, _, deliverable, _)| *deliverable && *canonical == needle)
    }
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            telegram: HashMap::new(),
            webhook: HashMap::new(),
            mqtt: HashMap::new(),
            filesystem: HashMap::new(),
            show_tool_calls: false,
            session_persistence: true,
            session_ttl_hours: 0,
        }
    }
}

/// Streaming mode for channels that support progressive message updates.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    /// No streaming -- send the complete response as a single message (default).
    #[default]
    Off,
    /// Update a draft message with every flush interval.
    Partial,
    /// Send the response as multiple separate messages at paragraph boundaries.
    #[serde(rename = "multi_message")]
    MultiMessage,
}

fn default_draft_update_interval_ms() -> u64 {
    1000
}

fn default_telegram_approval_timeout_secs() -> u64 {
    120
}

pub const TELEGRAM_OFFICIAL_API_BASE_URL: &str = "https://api.telegram.org";

fn default_telegram_api_base_url() -> String {
    TELEGRAM_OFFICIAL_API_BASE_URL.to_string()
}

/// Telegram bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.telegram"]
pub struct TelegramConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Telegram Bot API token (from @BotFather). `#[serde(default)]` so a
    /// config that omits or later has it pruned (e.g. a freshly created
    /// alias with an empty token, stripped by `prune_empty_leaves` before
    /// write) still deserializes as an empty string - instead of failing
    /// with `missing field 'bot_token'` and getting dropped by the resilient
    /// salvage pass. `validate_bot_token` below still requires a real token
    /// once `enabled = true`.
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[serde(default)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bot_token: String,
    /// Telegram Bot API base URL. Defaults to the official Telegram endpoint;
    /// set to a local Bot API server URL when self-hosting Telegram's bot API.
    #[tab(Connection)]
    #[serde(default = "default_telegram_api_base_url")]
    pub api_base_url: String,
    /// Streaming mode for progressive response delivery via message edits.
    #[tab(Behavior)]
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval (ms) between draft message edits to avoid rate limits.
    #[tab(Behavior)]
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// Inbound message debounce window in milliseconds for this Telegram alias.
    /// When set, overrides the global `[channels].debounce_ms` for this channel
    /// only. `0` or unset falls back to the global value.
    #[tab(Behavior)]
    #[serde(default)]
    pub debounce_ms: Option<u64>,
    /// When true, a newer Telegram message from the same sender in the same chat
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[tab(Behavior)]
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages are always processed.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
    /// Override for the top-level `ack_reactions` setting. When `None`, the
    /// channel falls back to `[channels].ack_reactions`. When set
    /// explicitly, it takes precedence.
    #[tab(Behavior)]
    #[serde(default)]
    pub ack_reactions: Option<bool>,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// How long (seconds) to wait for the operator to tap an inline-keyboard
    /// button on a tool approval prompt before auto-denying. Default: 120.
    #[tab(Behavior)]
    #[serde(default = "default_telegram_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            api_base_url: default_telegram_api_base_url(),
            stream_mode: StreamMode::default(),
            draft_update_interval_ms: default_draft_update_interval_ms(),
            interrupt_on_new_message: false,
            mention_only: false,
            ack_reactions: None,
            proxy_url: None,
            approval_timeout_secs: default_telegram_approval_timeout_secs(),
            excluded_tools: Vec::new(),
            reply_min_interval_secs: 0,
            reply_queue_depth_max: 0,
            debounce_ms: None,
        }
    }
}

impl ChannelConfig for TelegramConfig {
    fn name() -> &'static str {
        "Telegram"
    }
    fn desc() -> &'static str {
        "connect your bot"
    }
}

/// How the Mattermost channel receives inbound messages.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum MattermostListenMode {
    /// Poll REST API every 3 seconds. The default — all existing configs
    /// continue to work without changes.
    #[default]
    Polling,
    /// Connect to `/api/v4/websocket` for near-real-time event delivery.
    Websocket,
}

/// Webhook channel configuration.
///
/// Receives messages via HTTP POST and sends replies to a configurable outbound URL.
/// This is the "universal adapter" for any system that supports webhooks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.webhook"]
pub struct WebhookConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Port to listen on for incoming webhooks.
    #[tab(Advanced)]
    #[serde(default = "default_webhook_channel_port")]
    pub port: u16,
    /// URL path to listen on (default: `/webhook`).
    #[tab(Advanced)]
    #[serde(default)]
    pub listen_path: Option<String>,
    /// URL to POST/PUT outbound messages to.
    #[tab(Advanced)]
    #[serde(default)]
    pub send_url: Option<String>,
    /// HTTP method for outbound messages (`POST` or `PUT`). Default: `POST`.
    #[tab(Advanced)]
    #[serde(default)]
    pub send_method: Option<String>,
    /// Optional `Authorization` header value for outbound requests.
    #[tab(Connection)]
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub auth_header: Option<String>,
    /// Shared secret for webhook signature verification (HMAC-SHA256).
    /// The channel will refuse to start without one. Set
    /// `[channels.webhook.<alias>].secret` in config.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub secret: Option<String>,

    /// DANGEROUS: accept legacy body-only signatures without timestamp or nonce checks.
    /// Default: false (replay protection enabled).
    #[tab(Advanced)]
    #[serde(default)]
    pub dangerously_disable_replay_protection: bool,
    /// Allowed timestamp skew in seconds, 1..=86400. Default: 300 (five minutes).
    #[tab(Advanced)]
    #[serde(default)]
    pub replay_window_secs: Option<u64>,
    /// Maximum remembered event IDs per listener, 1..=65536. Default: 4096.
    /// A full cache rejects new events until records expire; live entries are never evicted.
    #[tab(Advanced)]
    #[serde(default)]
    pub replay_cache_capacity: Option<usize>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,

    /// Maximum number of retry attempts for outbound sends on transient failures
    /// (network errors, 429, 5xx). Set to `0` to disable retries. Default: `3`.
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Base delay in milliseconds for exponential backoff between retries. Default: `500`.
    /// Values below `1` are clamped to `1ms` at runtime to avoid busy-retry loops.
    #[serde(default)]
    pub retry_base_delay_ms: Option<u64>,
    /// Maximum delay cap in milliseconds for any single retry wait. Default: `30000` (30s).
    /// Values below `1` are clamped to `1ms` at runtime to avoid busy-retry loops.
    #[serde(default)]
    pub retry_max_delay_ms: Option<u64>,
}

fn default_webhook_channel_port() -> u16 {
    8090
}

impl ChannelConfig for WebhookConfig {
    fn name() -> &'static str {
        "Webhook"
    }
    fn desc() -> &'static str {
        "HTTP endpoint"
    }
}

impl_reply_pacing!(TelegramConfig, WebhookConfig);

// ── Security Config ─────────────────────────────────────────────────

/// Security configuration for audit logging, credential redaction, OTP, and e-stop.
///
/// Sandbox backend and resource limits live on per-agent risk profiles
/// (see `RiskProfileConfig::sandbox_*` and `RiskProfileConfig::max_*`); the
/// runtime resolves them via `Config::active_risk_profile(agent_alias)`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security"]
pub struct SecurityConfig {
    /// Audit logging configuration
    #[serde(default)]
    #[nested]
    pub audit: AuditConfig,

    /// Outbound credential leak detection and redaction configuration. See
    /// `[security.leak_detection]` for the detector controls.
    #[serde(default)]
    #[nested]
    pub leak_detection: LeakDetectionConfig,

    /// OTP gating configuration for sensitive actions/domains.
    #[serde(default)]
    #[nested]
    pub otp: OtpConfig,

    /// Emergency-stop state machine configuration.
    #[serde(default)]
    #[nested]
    pub estop: EstopConfig,
}

/// Outbound credential leak detection configuration.
///
/// These settings control the final guardrail pass over outbound channel
/// responses before they are delivered. Deterministic credential patterns
/// include API keys, private keys, database URLs, bot tokens, and related
/// token syntax. The high-entropy pass is a separate heuristic for standalone
/// opaque tokens.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.leak_detection"]
pub struct LeakDetectionConfig {
    /// Enable outbound credential leak detection and redaction.
    #[serde(default = "default_leak_detection_enabled")]
    pub enabled: bool,

    /// Detection sensitivity from 0.0 to 1.0; higher is more aggressive.
    #[serde(default = "default_leak_detection_sensitivity")]
    pub sensitivity: f64,

    /// Enable high-entropy token redaction; deterministic patterns still run when false.
    #[serde(default = "default_leak_detection_high_entropy_tokens")]
    pub high_entropy_tokens: bool,
}

fn default_leak_detection_enabled() -> bool {
    true
}

fn default_leak_detection_sensitivity() -> f64 {
    0.7
}

fn default_leak_detection_high_entropy_tokens() -> bool {
    true
}

impl Default for LeakDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: default_leak_detection_enabled(),
            sensitivity: default_leak_detection_sensitivity(),
            high_entropy_tokens: default_leak_detection_high_entropy_tokens(),
        }
    }
}

/// OTP validation strategy.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, reiclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum OtpMethod {
    /// Time-based one-time password (RFC 6238).
    #[default]
    Totp,
    /// Future method for paired-device confirmations.
    Pairing,
    /// Future method for local CLI challenge prompts.
    CliPrompt,
}

/// Security OTP configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.otp"]
#[serde(deny_unknown_fields)]
pub struct OtpConfig {
    /// Enable OTP gating. Defaults to disabled for backward compatibility.
    #[serde(default)]
    pub enabled: bool,

    /// OTP method.
    #[serde(default)]
    pub method: OtpMethod,

    /// TOTP time-step in seconds.
    #[serde(default = "default_otp_token_ttl_secs")]
    pub token_ttl_secs: u64,

    /// Reuse window for recently validated OTP codes.
    #[serde(default = "default_otp_cache_valid_secs")]
    pub cache_valid_secs: u64,

    /// Deprecated and unsupported: ZeroClaw has no OTP action-gating, so
    /// this list is parsed for backward compatibility but never enforced
    /// and provides no authorization boundary. Malformed entries still
    /// fail validation for compatibility; a non-default value emits an
    /// `otp_action_gating_unsupported` config-health warning. High-risk
    /// action authorization is owned by the Tachi approval/grant plane
    /// with Node-local enforcement.
    #[serde(default = "default_otp_gated_actions")]
    pub gated_actions: Vec<String>,

    /// Deprecated and unsupported: no runtime path matches domains against
    /// OTP policy, so this list is parsed for backward compatibility but
    /// never enforced. A non-empty value emits an
    /// `otp_action_gating_unsupported` config-health warning (see
    /// `gated_actions` for the authorization owner).
    #[serde(default)]
    pub gated_domains: Vec<String>,

    /// Deprecated and unsupported: like `gated_domains`, parsed for
    /// backward compatibility but never enforced. A non-empty value emits
    /// an `otp_action_gating_unsupported` config-health warning.
    #[serde(default)]
    pub gated_domain_categories: Vec<String>,

    /// Deprecated and unsupported: no OTP challenge-attempt limiter
    /// consumes this value, so it is parsed for backward compatibility but
    /// never enforced. `0` still fails validation for compatibility; any
    /// other non-default value emits an `otp_action_gating_unsupported`
    /// config-health warning.
    #[serde(default = "default_otp_challenge_max_attempts")]
    pub challenge_max_attempts: u32,
}

fn default_otp_token_ttl_secs() -> u64 {
    30
}

fn default_otp_cache_valid_secs() -> u64 {
    300
}

pub(crate) fn default_otp_challenge_max_attempts() -> u32 {
    3
}

pub(crate) fn default_otp_gated_actions() -> Vec<String> {
    vec![
        "shell".to_string(),
        "file_write".to_string(),
        "memory_forget".to_string(),
    ]
}

impl Default for OtpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: OtpMethod::Totp,
            token_ttl_secs: default_otp_token_ttl_secs(),
            cache_valid_secs: default_otp_cache_valid_secs(),
            gated_actions: default_otp_gated_actions(),
            gated_domains: Vec::new(),
            gated_domain_categories: Vec::new(),
            challenge_max_attempts: default_otp_challenge_max_attempts(),
        }
    }
}

/// Emergency stop configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.estop"]
#[serde(deny_unknown_fields)]
pub struct EstopConfig {
    /// Enable emergency stop controls.
    #[serde(default)]
    pub enabled: bool,

    /// File path used to persist estop state.
    #[serde(default = "default_estop_state_file")]
    pub state_file: String,

    /// Require a valid OTP before resume operations.
    #[serde(default = "default_true")]
    pub require_otp_to_resume: bool,
}

fn default_estop_state_file() -> String {
    default_path_under_config_dir("estop-state.json")
}

impl Default for EstopConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            state_file: default_estop_state_file(),
            require_otp_to_resume: true,
        }
    }
}

/// Sandbox configuration for OS-level isolation
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.sandbox"]
pub struct SandboxConfig {
    /// Enable sandboxing (None = auto-detect, Some = explicit)
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Sandbox backend to use
    #[serde(default)]
    pub backend: SandboxBackend,

    /// Custom Firejail arguments (when backend = firejail)
    #[serde(default)]
    pub firejail_args: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: None, // Auto-detect
            backend: SandboxBackend::Auto,
            firejail_args: Vec::new(),
        }
    }
}

/// Sandbox backend selection
#[derive(Debug, Clone, Serialize, Deserialize, Default, reiclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SandboxBackend {
    /// Auto-detect best available (default)
    #[default]
    Auto,
    /// Landlock (Linux kernel LSM, native)
    Landlock,
    /// Firejail (user-space sandbox)
    Firejail,
    /// Bubblewrap (user namespaces)
    Bubblewrap,
    /// Docker container isolation
    Docker,
    /// macOS sandbox-exec (Seatbelt)
    #[serde(alias = "sandbox-exec")]
    SandboxExec,
    /// No sandboxing (application-layer only)
    None,
}

/// Audit logging configuration
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.audit"]
pub struct AuditConfig {
    /// Enable audit logging
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,

    /// Path to audit log file (relative to zeroclaw dir)
    #[serde(default = "default_audit_log_path")]
    pub log_path: String,

    /// Maximum log size in MB before rotation
    #[serde(default = "default_audit_max_size_mb")]
    pub max_size_mb: u32,

    /// Sign events with HMAC for tamper evidence
    #[serde(default)]
    pub sign_events: bool,
}

fn default_audit_enabled() -> bool {
    true
}

fn default_audit_log_path() -> String {
    "audit.log".to_string()
}

fn default_audit_max_size_mb() -> u32 {
    100
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            log_path: default_audit_log_path(),
            max_size_mb: default_audit_max_size_mb(),
            sign_events: false,
        }
    }
}

// ── Security ops config ─────────────────────────────────────────

/// Managed Cybersecurity Service (MCSS) dashboard agent configuration (`[security_ops]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security_ops"]
pub struct SecurityOpsConfig {
    /// Enable security operations support. This no longer registers a
    /// model-callable tool: the diagnostics module stays compiled but is
    /// unreachable from every assembly path, and no operator surface
    /// exists for it. The flag is kept parseable so existing config files
    /// load unchanged; the daemon notes the withheld section at startup.
    #[serde(default)]
    pub enabled: bool,
    /// Directory containing incident response playbook definitions (JSON).
    #[serde(default = "default_playbooks_dir")]
    pub playbooks_dir: String,
    /// Automatically triage incoming alerts without user prompt.
    #[serde(default)]
    pub auto_triage: bool,
    /// Require human approval before executing playbook actions.
    #[serde(default = "default_require_approval")]
    pub require_approval_for_actions: bool,
    /// Maximum severity level that can be auto-remediated without approval.
    /// One of: "low", "medium", "high", "critical". Default: "low".
    #[serde(default = "default_max_auto_severity")]
    pub max_auto_severity: String,
    /// Directory for generated security reports.
    #[serde(default = "default_report_output_dir")]
    pub report_output_dir: String,
    /// Optional SIEM webhook URL for alert ingestion.
    #[serde(default)]
    pub siem_integration: Option<String>,
}

fn default_playbooks_dir() -> String {
    default_path_under_config_dir("playbooks")
}

fn default_require_approval() -> bool {
    true
}

fn default_max_auto_severity() -> String {
    "low".into()
}

fn default_report_output_dir() -> String {
    default_path_under_config_dir("security-reports")
}

impl Default for SecurityOpsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            playbooks_dir: default_playbooks_dir(),
            auto_triage: false,
            require_approval_for_actions: true,
            max_auto_severity: default_max_auto_severity(),
            report_output_dir: default_report_output_dir(),
            siem_integration: None,
        }
    }
}

// ── Config impl ──────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        let home =
            UserDirs::new().map_or_else(|| PathBuf::from("."), |u| u.home_dir().to_path_buf());
        let rei_dir = home.join(".rei");

        Self {
            data_dir: rei_dir.join("data"),
            config_path: rei_dir.join("config.toml"),
            composition: None,
            tasks: crate::tasks::TasksConfig::default(),
            env_overridden_paths: EnvOverriddenPaths::new(),
            pre_override_snapshots: PreOverrideSnapshots::new(),
            onepassword_reference_snapshots: ExternalRefSnapshots::new(),
            dirty_paths: std::collections::HashSet::new(),
            dirty_map_entries: std::collections::HashSet::new(),
            explicit_prop_dirty_paths: std::collections::HashSet::new(),
            tracked_prop_identities: std::collections::HashMap::new(),
            degraded_security: Vec::new(),
            degraded_sections: Vec::new(),
            retired_surface_warnings: Vec::new(),
            loaded_from: None,
            schema_version: crate::current::CURRENT_SCHEMA_VERSION,
            providers: crate::providers::Providers::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            observability: ObservabilityConfig::default(),
            security: SecurityConfig::default(),
            security_ops: SecurityOpsConfig::default(),
            runtime: RuntimeConfig::default(),
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            pacing: PacingConfig::default(),
            skills: SkillsConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            cron: HashMap::new(),
            channels: ChannelsConfig::default(),
            memory: MemoryConfig::default(),
            storage: StorageConfig::default(),
            gateway: GatewayConfig::default(),
            secrets: SecretsConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            web_fetch: WebFetchConfig::default(),
            web_search: WebSearchConfig::default(),
            proxy: ProxyConfig::default(),
            cost: CostConfig::default(),
            agents: HashMap::new(),
            risk_profiles: HashMap::new(),
            runtime_profiles: HashMap::new(),
            personas: HashMap::new(),
            cards: HashMap::new(),
            companion_memory: crate::companion::CompanionMemoryConfig::default(),
            skill_bundles: HashMap::new(),
            mcp_bundles: HashMap::new(),
            peer_groups: HashMap::new(),
            hooks: HooksConfig::default(),
            query_classification: QueryClassificationConfig::default(),
            transcription: TranscriptionConfig::default(),
            tts: TtsConfig::default(),
            mcp: McpConfig::default(),
            tachi: crate::tachi::TachiConfig::default(),
            nodes: NodesConfig::default(),
            onboard_state: OnboardStateConfig::default(),
            verifiable_intent: VerifiableIntentConfig::default(),
            sop: HashMap::new(),
            shell_tool: ShellToolConfig::default(),
            escalation: EscalationConfig::default(),
        }
    }
}

impl Config {
    /// External-peer usernames authorized on `<channel_type>.<alias>`.
    ///
    /// A `[peer_groups.<name>]` contributes when its `channel` field either
    /// matches `channel_type` (type-wide group, applies to every alias of
    /// that type) or matches the full dotted `"<channel_type>.<alias>"`
    /// (instance-scoped group, applies to that one alias only).
    pub fn channel_external_peers(&self, channel_type: &str, alias: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for group in self.peer_groups.values() {
            let group_matches = match group.channel.split_once('.') {
                Some((ty, al)) => ty == channel_type && al == alias,
                None => group.channel == channel_type,
            };
            if !group_matches {
                continue;
            }
            for peer in &group.external_peers {
                let username = peer.as_str().to_string();
                if seen.insert(username.clone()) {
                    out.push(username);
                }
            }
        }
        out
    }

    /// Voice-peer usernames for `<channel_type>.<alias>` that should always
    /// receive TTS voice replies.
    ///
    /// A `[peer_groups.<name>]` contributes when its `channel` field matches
    /// (type-wide or dotted) **and** `output_modality = "voice"`.
    ///
    /// This is the live-resolve counterpart of `channel_external_peers`,
    /// filtered to voice-only peer groups. No cache — single source of truth
    /// is `self.peer_groups`.
    pub fn channel_voice_peers(&self, channel_type: &str, alias: &str) -> Vec<String> {
        use crate::multi_agent::OutputModality;

        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for group in self.peer_groups.values() {
            if group.output_modality != OutputModality::Voice {
                continue;
            }
            let group_matches = match group.channel.split_once('.') {
                Some((ty, al)) => ty == channel_type && al == alias,
                None => group.channel == channel_type,
            };
            if !group_matches {
                continue;
            }
            for peer in &group.external_peers {
                let username = peer.as_str().to_string();
                if seen.insert(username.clone()) {
                    out.push(username);
                }
            }
        }
        out
    }

    /// Sender usernames authorized to issue `/model --agent <model>` on
    /// `<channel_type>.<alias>` for agent `agent_alias`. A
    /// `[peer_groups.<name>]` contributes when its `channel` matches
    /// (type-wide or dotted), its `admin_for_agent_scope = true`, and its
    /// `agents` list (if non-empty) contains `agent_alias`. Returns
    /// deduped and sorted. Live-resolve against the `Config` this method
    /// is called on — no internal cache. Edits to `peer_groups` take
    /// effect at the next `Config::channel_agent_scope_admins` call; the
    /// orchestrator gate reads through a snapshot of this `Config`, so
    /// operator edits become visible after the runtime context is rebuilt
    /// (daemon restart).
    pub fn channel_agent_scope_admins(
        &self,
        channel_type: &str,
        alias: &str,
        agent_alias: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for group in self.peer_groups.values() {
            if !group.admin_for_agent_scope {
                continue;
            }
            let group_matches = match group.channel.split_once('.') {
                Some((ty, al)) => ty == channel_type && al == alias,
                None => group.channel == channel_type,
            };
            if !group_matches {
                continue;
            }
            // Agent-bound: when the group's `agents` list is non-empty,
            // only grant the privilege if `agent_alias` appears in it.
            // An empty `agents` list is interpreted as "all agents"
            // (backward-compatible channel-wide default).
            if !group.agents.is_empty() && !group.agents.iter().any(|a| a.as_str() == agent_alias) {
                continue;
            }
            for peer in &group.external_peers {
                let username = peer.as_str().to_string();
                if seen.insert(username.clone()) {
                    out.push(username);
                }
            }
        }
        out.sort();
        out
    }

    /// Collect the `IntegrationDescriptor` from every nested config that
    /// declares one via `#[integration(...)]`. Adding a new toggleable
    /// integration is one struct-level attribute on the new config + one
    /// row in this method. The integrations registry consumes the result
    /// without per-vendor branches.
    pub fn integration_descriptors(&self) -> Vec<crate::config::IntegrationDescriptor> {
        // so the macro emits `integration_descriptor()` on it (BrowserConfig
        // was the exemplar; it retired with the browser tools). Cron has been flattened
        // to `HashMap<String, CronJobDecl>` with no enable toggle, so
        // it gets a hand-crafted descriptor whose `active` reflects
        // whether any job is configured. Display copy lives next to
        // the field so the registry never branches on a category name.
        vec![crate::config::IntegrationDescriptor {
            display_name: "Cron",
            description: "Scheduled tasks",
            category: "ToolsAutomation",
            active: !self.cron.is_empty(),
        }]
    }

    /// Returns `true` if `path` was populated by a `REI_*` env-var
    /// override at load time. O(1) HashSet lookup; safe to call per row in
    /// list-rendering paths (`config list`, dashboard, quickstart).
    pub fn prop_is_env_overridden(&self, path: &str) -> bool {
        self.env_overridden_paths.contains(path)
    }

    /// Walk every channel HashMap that carries reply pacing fields and yield
    /// `(dotted-path-prefix, &dyn HasReplyPacing)` pairs. Used by validation
    /// and by anywhere that wants a single source of truth for which channel
    /// types participate in pacing.
    pub fn reply_pacing_entries(&self) -> Vec<(String, &dyn HasReplyPacing)> {
        let c = &self.channels;
        fn rows<'a, C: HasReplyPacing>(
            ch_type: &'static str,
            map: &'a std::collections::HashMap<String, C>,
        ) -> impl Iterator<Item = (String, &'a dyn HasReplyPacing)> + 'a {
            map.iter().map(move |(alias, cfg)| {
                (
                    format!("channels.{ch_type}.{alias}"),
                    cfg as &dyn HasReplyPacing,
                )
            })
        }
        rows("telegram", &c.telegram)
            .chain(rows("webhook", &c.webhook))
            .collect()
    }
}

impl HasPropKind for serde_json::Value {
    // `serde_json::Value` is an arbitrary JSON document, not an enum.
    // Classifying it as `Enum` previously made `enum_variants_for::<Value>()`
    // hand back the literal placeholder `"(unknown variants)"`, and the
    // dashboard form rendered fields like `model_providers.<key>.provider_extra`
    // as a single-option dropdown. `String` is the closest scalar kind —
    // the form renders a text input where the user pastes raw JSON.
    // Round-trip via `set_prop` stays correct: serde deserializes the TOML
    // string back into `Value::String(...)`. Power users editing complex
    // objects still use `rei config set --json` or hand-edit the
    // `config.toml`.
    const PROP_KIND: PropKind = PropKind::String;
}

#[cfg(test)]
mod tests;
