//! Confined sleep-time memory compactor and `04:00` Auto Dream worker (ADR-025).
//!
//! Runs a strictly confined tool-call loop (`run_tool_call_loop` + `ProviderDispatch`)
//! over ONLY the seven sleep primitives (`session_search`, `episode_read`,
//! `checkpoint_stage`, `episode_commit`, `memory_supersede`, `digest_publish`,
//! `action_propose`) directly backed by `&dyn SessionBackend` and `&dyn Memory`.
//!
//! Hard confinement invariants:
//! - Zero shell, process, HTTP, network, or workspace file tools.
//! - Zero write access to `soul.db` or `user_model.db`.
//! - Action proposals are strictly `ActionCandidateStatus::Pending` items inside
//!   `DailyDigest.action_candidates` / `action_decisions` requiring explicit
//!   owner confirmation.
//! - Existing `owner_edited = true` `DailyDigest`s for a `day_key` are never
//!   overwritten by machine runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeroclaw_api::attribution::ToolKind;
use zeroclaw_api::memory_traits::{
    ActionCandidateStatus, DailyDigest, EpisodeRecord, EpisodeSourceKind, Memory, MemoryCategory,
    MemoryEntry, MemoryKind, PINNED_DIGEST_MAX_BYTES, PinnedDigest, SemanticSubtype, StoreOptions,
    validate_agent_id, validate_non_soul_key, validate_non_soul_namespace,
};
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::attention::AttentionConfig;
use zeroclaw_config::multi_agent::MemoryBackendKind;
use zeroclaw_config::schema::{Config, MemoryConfig, MultimodalConfig, PacingConfig};
use zeroclaw_infra::session_backend::{SessionBackend, SessionCheckpoint, SessionMetadata};
use zeroclaw_memory::conflict;
use zeroclaw_memory::dedup::{self, DedupAction};
use zeroclaw_memory::importance;
use zeroclaw_memory::merge;
use zeroclaw_memory::policy::PolicyEnforcer;
use zeroclaw_memory::policy_gate;
use zeroclaw_providers::{ChatMessage, ModelProvider, ProviderDispatch};

use crate::agent::history::{estimate_history_tokens, floor_char_boundary};
use crate::agent::loop_::{
    LoopKnobs, ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess, ResolvedRuntimeKnobs,
    ToolLoop, run_tool_call_loop,
};
use crate::agent::session_model_provider::build_session_model_provider;
use crate::observability::NoopObserver;

/// Context-budget utilization percentage that triggers staging an L1 checkpoint.
pub const WATERMARK_STAGE_PERCENT: usize = 65;

/// Context-budget utilization percentage that promotes the staged L1 checkpoint.
pub const WATERMARK_PROMOTE_PERCENT: usize = 90;

/// Minimum inactivity window (15 minutes) before a staged checkpoint promotes
/// or an idle session flushes unflushed turns into an `EpisodeRecord`.
pub const COLD_WINDOW_GAP_SECS: i64 = 900;

/// Periodic daemon check interval for idle session flushes and `04:00` Auto Dream.
pub const SLEEP_CHECK_INTERVAL: Duration = Duration::from_secs(300);

/// Local hour for the daily Auto Dream day cut (`04:00`).
pub const DREAM_CUT_HOUR: i8 = 4;

/// Maximum atomic Core facts persisted per compaction pass.
const MAX_TYPED_FACTS_PER_PASS: usize = 5;

/// Lightweight JSON state marker file inside `config.data_dir` tracking
/// the last completed Auto Dream `day_key` per agent (no second SQLite DB).
const DREAM_STATE_FILENAME: &str = "sleep_dream_state.json";

/// Confined tool names allowed inside the sleep compactor tool loop.
pub const CONFINED_SLEEP_TOOL_NAMES: &[&str] = &[
    "session_search",
    "episode_read",
    "checkpoint_stage",
    "episode_commit",
    "memory_supersede",
    "digest_publish",
    "action_propose",
];

const FAST_ROUTE_HINT: &str = "fast";

const WATERMARK_COMPACTOR_SYSTEM_PROMPT: &str = r#"You are a confined sleep-time memory compactor.
Your ONLY job is to distill conversation turns into a crisp rolling checkpoint and structured episode memory without losing reasoning or tradeoffs.

Rules:
1. Preserve user and assistant Thoughts (`why_and_tradeoffs`), `settled_decisions`, and `rejected_options` — never flatten decisions into bare outcomes without why they were chosen or what alternatives were rejected.
2. Call `checkpoint_stage` with a concise, lossless rolling digest of the conversation window up to the provided message watermark.
3. Call `episode_commit` with structured `summary`, `why_and_tradeoffs`, `settled_decisions`, `rejected_options`, `entities`, and optional `core_facts` (only durable facts/preferences/decisions; never reusable how-to procedures).
4. If an earlier Core fact was explicitly contradicted or replaced, call `memory_supersede`.
5. Never execute actions, never modify identity/soul rules, and never invent facts absent from the transcript.

You may also return a concise JSON summary if no additional tool calls are needed."#;

const EPISODE_FLUSH_SYSTEM_PROMPT: &str = r#"You are a confined sleep-time episode compiler.
Distill the provided unflushed session transcript into a durable `EpisodeRecord` via `episode_commit` (and `memory_supersede` if a prior Core fact was explicitly superseded).

Preserve:
- User and assistant Thoughts (`why_and_tradeoffs`)
- `settled_decisions`
- `rejected_options`
- `entities`
- Durable `core_facts` (if any)

Never propose or execute external actions here unless clearly warranted; keep summaries grounded strictly in the transcript."#;

const DEEP_DREAM_SYSTEM_PROMPT: &str = r#"You are the overnight 04:00 Deep Auto Dream synthesizer for a single personal agent.
You operate ONLY over the agent's committed episodes and scoped session transcripts using your confined sleep tools (`session_search`, `episode_read`, `memory_supersede`, `action_propose`, `digest_publish`).

Instructions:
1. Review the day's episodes carefully, paying special attention to user & assistant Thoughts (`why_and_tradeoffs`), `settled_decisions`, and `rejected_options`.
2. Reconcile any contradicted or outdated Core facts using `memory_supersede` (never touch `soul` keys or namespaces).
3. Surface any actionable follow-ups strictly as pending proposals via `action_propose` — NEVER execute actions; all candidates remain `Pending` until explicit owner confirmation.
4. Call `digest_publish` once with:
   - `narrative`: clear synthesis of the day
   - `settled_decisions`: durable decisions settled
   - `why_and_tradeoffs`: preserved Thoughts, reasoning, and tradeoffs
   - `latent_insights`: subtle cross-episode connections or open tensions
   - `entities`: key people, systems, files, or concepts
   - `pinned_index`: updated L0 index (<= 8000 UTF-8 bytes) summarizing active projects, settled decisions, and entity anchors.

If an owner-edited DailyDigest already exists for this day_key, `digest_publish` will preserve the owner's edits automatically."#;

const TURN_PARITY_SYSTEM_PROMPT: &str = r#"You are a confined sleep-time memory compactor.
Given one conversation turn, call `episode_commit` to record:
- `summary`: brief 1-2 sentence summary of what happened
- `why_and_tradeoffs`: any user/assistant reasoning or tradeoffs
- `settled_decisions`: decisions settled in the turn
- `rejected_options`: options considered and rejected
- `primary_memory_update`: any NEW durable fact, preference, decision, or commitment worth remembering long-term (or null)
- `primary_kind`: "preference", "fact", "decision", or "entity" (or null when primary_memory_update is null)
- `core_facts`: array of atomic durable facts `{"content": "...", "kind": "preference"|"fact"|"decision"|"entity"}` (empty if none; exclude procedures/how-to workflows)

If you cannot call tools, respond ONLY with valid JSON:
{"history_entry": "...", "memory_update": "..." or null, "kind": "preference"|"fact"|"decision"|"entity"|null, "facts": ["..."], "why_and_tradeoffs": ["..."], "settled_decisions": ["..."], "rejected_options": ["..."], "entities": ["..."]}"#;

static MEDIA_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[(?:IMAGE|DOCUMENT|FILE|VIDEO|VOICE|AUDIO):[^\]]*\]")
        .unwrap_or_else(|_| Regex::new("$^").unwrap_or_else(|_| unreachable!()))
});

fn strip_media_markers(text: &str) -> String {
    MEDIA_MARKER_RE
        .replace_all(text, "[media attachment]")
        .into_owned()
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = floor_char_boundary(text, max_bytes);
    format!("{}…", &text[..end])
}

/// Parse a model-emitted semantic subtype, defaulting safely to `Fact`.
pub fn parse_semantic_subtype(raw: &str) -> SemanticSubtype {
    match raw.trim().to_ascii_lowercase().as_str() {
        "preference" | "pref" | "user_preference" => SemanticSubtype::Preference,
        "decision" | "decided" => SemanticSubtype::Decision,
        "entity" | "person" | "place" | "organization" | "org" => SemanticSubtype::Entity,
        "fact" | "" => SemanticSubtype::Fact,
        _ => SemanticSubtype::Fact,
    }
}

/// Resolved model binding for a sleep pass (D8 split routing).
pub struct ResolvedSleepModel {
    pub provider: Box<dyn ModelProvider>,
    pub provider_name: String,
    pub model: String,
    pub temperature: Option<f64>,
}

/// Resolve the fast/hinted model route (`hint:fast` in `config.model_routes` if
/// configured, falling back to the agent's live primary model) for watermark
/// checkpointing and idle/startup Episode flushes.
///
/// Honors live agent provider isolation: if `live_provider_ref` does not
/// resolve in `config.providers.models`, returns `None` rather than rerouting
/// to an unrelated default provider.
pub fn resolve_fast_sleep_model(
    config: &Config,
    live_provider_ref: &str,
    live_model: &str,
) -> Option<ResolvedSleepModel> {
    let (family, alias) = live_provider_ref.split_once('.')?;
    let primary_entry = config.providers.models.find(family, alias)?;
    let (provider, provider_name, resolved_primary_model) =
        build_session_model_provider(config, live_provider_ref, Some(live_model)).ok()?;

    if let Some(fast_route) = config
        .model_routes
        .iter()
        .find(|route| route.hint.trim() == FAST_ROUTE_HINT)
    {
        let fast_temp = fast_route
            .model_provider
            .split_once('.')
            .and_then(|(rf, ra)| config.providers.models.find(rf, ra))
            .and_then(|entry| entry.temperature)
            .or(primary_entry.temperature);
        return Some(ResolvedSleepModel {
            provider,
            provider_name,
            model: format!("hint:{FAST_ROUTE_HINT}"),
            temperature: fast_temp,
        });
    }

    Some(ResolvedSleepModel {
        provider,
        provider_name,
        model: resolved_primary_model,
        temperature: primary_entry.temperature,
    })
}

/// Resolve the agent's primary flagship model for `04:00` Deep Auto Dream or
/// daemon-side sleep passes.
pub fn resolve_primary_sleep_model(
    config: &Config,
    agent_alias: &str,
) -> Option<ResolvedSleepModel> {
    let (family, alias, entry) = config.resolved_model_provider_for_agent(agent_alias)?;
    let model = entry
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())?;
    let provider_ref = format!("{family}.{alias}");
    let (provider, provider_name, resolved_model) =
        build_session_model_provider(config, &provider_ref, Some(model)).ok()?;
    Some(ResolvedSleepModel {
        provider,
        provider_name,
        model: resolved_model,
        temperature: entry.temperature,
    })
}

/// Resolve the daemon fast model for `agent_alias` (`hint:fast` if configured,
/// falling back to the agent's primary model).
pub fn resolve_daemon_fast_sleep_model(
    config: &Config,
    agent_alias: &str,
) -> Option<ResolvedSleepModel> {
    let (family, alias, entry) = config.resolved_model_provider_for_agent(agent_alias)?;
    let model = entry
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())?;
    let provider_ref = format!("{family}.{alias}");
    resolve_fast_sleep_model(config, &provider_ref, model)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ExtractedCoreFact {
    content: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

#[derive(Debug, Default)]
struct SleepPassOutcome {
    staged_checkpoint: Option<SessionCheckpoint>,
    committed_episodes: Vec<String>,
    superseded_facts: Vec<String>,
    proposed_actions: Vec<String>,
    published_digest: Option<DailyDigest>,
    preserved_owner_digest: bool,
}

#[derive(Clone)]
struct SharedOutcome(Arc<Mutex<SleepPassOutcome>>);

impl SharedOutcome {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(SleepPassOutcome::default())))
    }
}

fn string_array_arg(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn is_unsupported_sleep_error(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("sleep memory writes unsupported by backend")
}

async fn store_core_fact_with_gates(
    memory: &dyn Memory,
    memory_config: &MemoryConfig,
    content: &str,
    explicit_kind: Option<&str>,
    explicit_key: Option<&str>,
    default_key_prefix: &str,
) -> Result<()> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if let Some(key) = explicit_key {
        validate_non_soul_key(key, "core_fact.key")?;
    }

    let policy = PolicyEnforcer::new(&memory_config.policy);
    if let Err(e) =
        policy_gate::validate_store(memory, &policy, "default", &MemoryCategory::Core).await
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"error": e.to_string()})),
            "sleep compactor core write denied by policy"
        );
        anyhow::bail!("sleep compactor core write denied by policy: {e}");
    }

    let kind_val = memory_config.types.enabled.then(|| {
        MemoryKind::Semantic(
            explicit_kind
                .map(parse_semantic_subtype)
                .unwrap_or(SemanticSubtype::Fact),
        )
    });

    let candidates = memory.recall(trimmed, 10, None, None, None).await?;
    let candidates = dedup::core_candidates(candidates);
    match dedup::dedup_gate(&candidates, trimmed, memory_config) {
        DedupAction::Insert => {}
        DedupAction::Reject { dup_of } => {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"duplicate_of": dup_of})),
                "sleep compactor skipped duplicate core update"
            );
            return Ok(());
        }
        DedupAction::Merge { into } => {
            if let Some(survivor) = candidates.iter().find(|entry| entry.id == into) {
                let merged = merge::merge_into_survivor(survivor, trimmed);
                let options = StoreOptions {
                    namespace: Some(survivor.namespace.clone()),
                    importance: merged.importance,
                    kind: survivor.kind.clone().or(kind_val),
                    pinned: survivor.pinned,
                    tenant_id: survivor.tenant_id.clone(),
                };
                memory
                    .store_with_options(
                        &survivor.key,
                        &merged.content,
                        MemoryCategory::Core,
                        survivor.session_id.as_deref(),
                        options,
                    )
                    .await?;
                return Ok(());
            }
        }
    }

    let mem_key = explicit_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{default_key_prefix}_{}", uuid::Uuid::new_v4()));
    let imp = importance::compute_importance(trimmed, &MemoryCategory::Core);

    let superseded_ids = match conflict::check_and_resolve_conflicts(
        memory,
        &mem_key,
        trimmed,
        &MemoryCategory::Core,
        memory_config.conflict_threshold,
    )
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                "sleep compactor conflict check skipped"
            );
            Vec::new()
        }
    };

    let options = StoreOptions {
        importance: Some(imp),
        kind: kind_val,
        ..StoreOptions::default()
    };
    memory
        .store_with_options(&mem_key, trimmed, MemoryCategory::Core, None, options)
        .await?;

    if memory_config.conflict_supersede_enabled
        && !superseded_ids.is_empty()
        && let Err(e) = memory.supersede(&superseded_ids, &mem_key).await
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{e}")})),
            "sleep compactor memory supersede skipped"
        );
    }

    Ok(())
}

fn fact_overlaps_primary(fact: &str, primary: &str, memory_config: &MemoryConfig) -> bool {
    let primary = primary.trim();
    if primary.is_empty() {
        return false;
    }
    if primary == fact {
        return true;
    }
    if !memory_config.dedup_on_write {
        return false;
    }
    let candidate = MemoryEntry {
        id: "primary_update_candidate".into(),
        key: "primary_update_candidate".into(),
        content: primary.to_string(),
        category: MemoryCategory::Core,
        timestamp: "now".into(),
        session_id: None,
        score: None,
        namespace: "default".into(),
        importance: None,
        superseded_by: None,
        kind: None,
        pinned: false,
        tenant_id: None,
        agent_alias: None,
        agent_id: None,
    };
    !matches!(
        dedup::dedup_gate(&[candidate], fact, memory_config),
        DedupAction::Insert
    )
}

async fn persist_extracted_core_facts(
    memory: &dyn Memory,
    memory_config: &MemoryConfig,
    primary_update: Option<&str>,
    primary_kind: Option<&str>,
    facts: &[ExtractedCoreFact],
) -> Result<()> {
    if memory_config.consolidation_extract_facts {
        let mut stored = 0usize;
        for fact in facts {
            let trimmed = fact.content.trim();
            if trimmed.is_empty() {
                continue;
            }
            if primary_update.is_some_and(|primary| fact_overlaps(trimmed, primary, memory_config))
            {
                continue;
            }
            let kind = fact.kind.as_deref().or(Some("fact"));
            store_core_fact_with_gates(
                memory,
                memory_config,
                trimmed,
                kind,
                fact.key.as_deref(),
                "core_fact",
            )
            .await?;
            stored += 1;
            if stored >= MAX_TYPED_FACTS_PER_PASS {
                break;
            }
        }
    }

    if let Some(primary) = primary_update.map(str::trim).filter(|s| !s.is_empty()) {
        store_core_fact_with_gates(memory, memory_config, primary, primary_kind, None, "core")
            .await?;
    } else if !memory_config.consolidation_extract_facts && memory_config.types.enabled {
        // When only `types.enabled` is on and the model emitted a single `core_facts`
        // item without `primary_memory_update`, promote the first fact as the primary Core write.
        if let Some(first) = facts.iter().find(|f| !f.content.trim().is_empty()) {
            store_core_fact_with_gates(
                memory,
                memory_config,
                first.content.trim(),
                first.kind.as_deref().or(primary_kind),
                first.key.as_deref(),
                "core",
            )
            .await?;
        }
    }

    Ok(())
}

fn fact_overlaps(fact: &str, primary: &str, memory_config: &MemoryConfig) -> bool {
    fact_overlaps_primary(fact, primary, memory_config)
}

// ── Confined Sleep Primitives (7 `Tool` implementations) ─────────────────────

struct SessionSearchTool {
    session_backend: Arc<dyn SessionBackend>,
    allowed_session_keys: Vec<String>,
}

zeroclaw_api::tool_attribution!(SessionSearchTool, ToolKind::Search);

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search persisted transcripts across this agent's scoped sessions using FTS5."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Keyword search query." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(query) = args.get("query").and_then(|v| v.as_str()).map(str::trim) else {
            return Ok(ToolResult::err("missing required string 'query'"));
        };
        if query.is_empty() {
            return Ok(ToolResult::err("'query' must not be empty"));
        }
        if self.allowed_session_keys.is_empty() {
            return Ok(ToolResult::ok(serde_json::json!({ "hits": [] })));
        }
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 50))
            .unwrap_or(10);
        let backend = Arc::clone(&self.session_backend);
        let keys = self.allowed_session_keys.clone();
        let query_owned = query.to_string();
        let hits = tokio::task::spawn_blocking(move || {
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            backend.search_session_fts(&key_refs, &query_owned, limit)
        })
        .await
        .context("session_search blocking task failed")??;
        Ok(ToolResult::ok(serde_json::json!({ "hits": hits })))
    }
}

struct EpisodeReadTool {
    memory: Arc<dyn Memory>,
    agent_id: String,
}

zeroclaw_api::tool_attribution!(EpisodeReadTool, ToolKind::Memory);

#[async_trait]
impl Tool for EpisodeReadTool {
    fn name(&self) -> &str {
        "episode_read"
    }

    fn description(&self) -> &str {
        "Read non-superseded EpisodeRecords for this agent within an optional RFC3339 time window or session."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "since": { "type": "string", "description": "Optional RFC 3339 lower bound." },
                "until": { "type": "string", "description": "Optional RFC 3339 upper bound." },
                "session_id": { "type": "string", "description": "Optional session key filter." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let since = args.get("since").and_then(|v| v.as_str());
        let until = args.get("until").and_then(|v| v.as_str());
        let session_id = args.get("session_id").and_then(|v| v.as_str());
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 100))
            .unwrap_or(20);
        let episodes = self
            .memory
            .read_episodes_in_window(&self.agent_id, since, until, session_id, limit)
            .await?;
        Ok(ToolResult::ok(serde_json::json!({ "episodes": episodes })))
    }
}

struct CheckpointStageTool {
    session_backend: Arc<dyn SessionBackend>,
    session_key: String,
    max_message_id: i64,
    outcome: SharedOutcome,
}

zeroclaw_api::tool_attribution!(CheckpointStageTool, ToolKind::Memory);

#[async_trait]
impl Tool for CheckpointStageTool {
    fn name(&self) -> &str {
        "checkpoint_stage"
    }

    fn description(&self) -> &str {
        "Stage a rolling L1 checkpoint digest for the bound session up to the verified message watermark."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "up_to_message_id": { "type": "integer", "minimum": 1 },
                "digest": { "type": "string", "description": "Compact rolling digest preserving decisions and reasoning." }
            },
            "required": ["digest"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(digest) = args
            .get("digest")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::err("'digest' must be a non-empty string"));
        };
        let requested_id = args
            .get("up_to_message_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(self.max_message_id);
        let watermark = requested_id.clamp(1, self.max_message_id.max(1));
        let backend = Arc::clone(&self.session_backend);
        let key = self.session_key.clone();
        let digest_owned = digest.to_string();
        let ckpt = tokio::task::spawn_blocking(move || {
            backend.stage_checkpoint(&key, watermark, &digest_owned)
        })
        .await
        .context("checkpoint_stage blocking task failed")??;
        self.outcome.0.lock().staged_checkpoint = Some(ckpt.clone());
        Ok(ToolResult::ok(serde_json::json!({ "checkpoint": ckpt })))
    }
}

struct EpisodeCommitTool {
    memory: Arc<dyn Memory>,
    memory_config: MemoryConfig,
    agent_id: String,
    default_session_key: String,
    source_kind: EpisodeSourceKind,
    default_started_at: String,
    default_ended_at: String,
    default_up_to_message_id: Option<i64>,
    tolerate_unsupported_sleep: bool,
    outcome: SharedOutcome,
}

zeroclaw_api::tool_attribution!(EpisodeCommitTool, ToolKind::Memory);

#[async_trait]
impl Tool for EpisodeCommitTool {
    fn name(&self) -> &str {
        "episode_commit"
    }

    fn description(&self) -> &str {
        "Commit a structured EpisodeRecord (preserving Thoughts/why_and_tradeoffs, settled_decisions, rejected_options) and optional Core typed facts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "episode_id": { "type": "string" },
                "session_key": { "type": "string" },
                "started_at": { "type": "string" },
                "ended_at": { "type": "string" },
                "summary": { "type": "string" },
                "why_and_tradeoffs": { "type": "array", "items": { "type": "string" } },
                "settled_decisions": { "type": "array", "items": { "type": "string" } },
                "rejected_options": { "type": "array", "items": { "type": "string" } },
                "entities": { "type": "array", "items": { "type": "string" } },
                "primary_memory_update": { "type": ["string", "null"] },
                "primary_kind": { "type": ["string", "null"] },
                "core_facts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "kind": { "type": "string" },
                            "key": { "type": "string" }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["summary"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(summary) = args
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::err("'summary' must be a non-empty string"));
        };
        let episode_id = args
            .get("episode_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("ep_{}", uuid::Uuid::new_v4()));
        validate_non_soul_key(&episode_id, "episode_id")?;

        let session_key = args
            .get("session_key")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.default_session_key)
            .to_string();
        validate_non_soul_key(&session_key, "session_key")?;

        let started_at = args
            .get("started_at")
            .and_then(|v| v.as_str())
            .filter(|s| zeroclaw_api::time::parse_rfc3339(s).is_ok())
            .map(str::to_string)
            .unwrap_or_else(|| self.default_started_at.clone());
        let mut ended_at = args
            .get("ended_at")
            .and_then(|v| v.as_str())
            .filter(|s| zeroclaw_api::time::parse_rfc3339(s).is_ok())
            .map(str::to_string)
            .unwrap_or_else(|| self.default_ended_at.clone());
        if let (Ok(s), Ok(e)) = (
            zeroclaw_api::time::parse_rfc3339(&started_at),
            zeroclaw_api::time::parse_rfc3339(&ended_at),
        ) && e < s
        {
            ended_at = started_at.clone();
        }

        let episode = EpisodeRecord {
            episode_id: episode_id.clone(),
            agent_id: self.agent_id.clone(),
            session_key,
            started_at,
            ended_at,
            up_to_message_id: self.default_up_to_message_id,
            summary: summary.to_string(),
            why_and_tradeoffs: string_array_arg(&args, "why_and_tradeoffs"),
            settled_decisions: string_array_arg(&args, "settled_decisions"),
            rejected_options: string_array_arg(&args, "rejected_options"),
            entities: string_array_arg(&args, "entities"),
            source_kind: self.source_kind,
        };
        episode.validate()?;

        let primary_update = args
            .get("primary_memory_update")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let primary_kind = args.get("primary_kind").and_then(|v| v.as_str());
        let core_facts: Vec<ExtractedCoreFact> = args
            .get("core_facts")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let stored_id = match self.memory.commit_episode(&episode).await {
            Ok(id) => id,
            Err(err) if self.tolerate_unsupported_sleep && is_unsupported_sleep_error(&err) => {
                let date = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
                let fallback_key = format!("daily_{date}_{}", uuid::Uuid::new_v4());
                self.memory
                    .store(&fallback_key, &episode.summary, MemoryCategory::Daily, None)
                    .await?;
                fallback_key
            }
            Err(err) => return Err(err),
        };

        persist_extracted_core_facts(
            self.memory.as_ref(),
            &self.memory_config,
            primary_update,
            primary_kind,
            &core_facts,
        )
        .await?;

        self.outcome.0.lock().committed_episodes.push(episode_id);
        Ok(ToolResult::ok(serde_json::json!({ "id": stored_id })))
    }
}

struct MemorySupersedeTool {
    memory: Arc<dyn Memory>,
    agent_id: String,
    outcome: SharedOutcome,
}

zeroclaw_api::tool_attribution!(MemorySupersedeTool, ToolKind::Memory);

#[async_trait]
impl Tool for MemorySupersedeTool {
    fn name(&self) -> &str {
        "memory_supersede"
    }

    fn description(&self) -> &str {
        "Atomically replace a contradicted Core fact (old_key -> new_key) within this agent's non-Soul namespace."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "old_key": { "type": "string" },
                "new_key": { "type": "string" },
                "new_content": { "type": "string" },
                "namespace": { "type": "string" }
            },
            "required": ["old_key", "new_key", "new_content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(old_key) = args.get("old_key").and_then(|v| v.as_str()).map(str::trim) else {
            return Ok(ToolResult::err("missing 'old_key'"));
        };
        let Some(new_key) = args.get("new_key").and_then(|v| v.as_str()).map(str::trim) else {
            return Ok(ToolResult::err("missing 'new_key'"));
        };
        let Some(new_content) = args
            .get("new_content")
            .and_then(|v| v.as_str())
            .map(str::trim)
        else {
            return Ok(ToolResult::err("missing 'new_content'"));
        };
        let namespace = args
            .get("namespace")
            .and_then(|v| v.as_str())
            .map(str::trim);

        validate_non_soul_key(old_key, "old_key")?;
        validate_non_soul_key(new_key, "new_key")?;
        validate_non_soul_namespace(namespace)?;

        let id = self
            .memory
            .supersede_fact(&self.agent_id, old_key, new_key, new_content, namespace)
            .await?;
        self.outcome
            .0
            .lock()
            .superseded_facts
            .push(format!("{old_key}->{new_key}"));
        Ok(ToolResult::ok(serde_json::json!({ "id": id })))
    }
}

struct ActionProposeTool {
    outcome: SharedOutcome,
}

zeroclaw_api::tool_attribution!(ActionProposeTool, ToolKind::Memory);

#[async_trait]
impl Tool for ActionProposeTool {
    fn name(&self) -> &str {
        "action_propose"
    }

    fn description(&self) -> &str {
        "Queue a pending follow-up action candidate for explicit owner review inside the DailyDigest. Never executes actions."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "description": "Short actionable candidate description for owner approval." },
                "rationale": { "type": "string" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(action) = args
            .get("action")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::err("'action' must not be empty"));
        };
        let formatted = match args
            .get("rationale")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(why) => format!("{action} — {why}"),
            None => action.to_string(),
        };
        let mut guard = self.outcome.0.lock();
        if !guard
            .proposed_actions
            .iter()
            .any(|existing| existing == &formatted)
        {
            guard.proposed_actions.push(formatted.clone());
        }
        Ok(ToolResult::ok(serde_json::json!({
            "status": "pending",
            "candidate": formatted,
        })))
    }
}

struct DigestPublishTool {
    memory: Arc<dyn Memory>,
    agent_id: String,
    day_key: String,
    window_start: String,
    window_end: String,
    outcome: SharedOutcome,
}

zeroclaw_api::tool_attribution!(DigestPublishTool, ToolKind::Memory);

#[async_trait]
impl Tool for DigestPublishTool {
    fn name(&self) -> &str {
        "digest_publish"
    }

    fn description(&self) -> &str {
        "Publish the daily synthesis (DailyDigest) and compact PinnedDigest (<= 8000 bytes), preserving any existing owner_edited DailyDigest."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "narrative": { "type": "string" },
                "settled_decisions": { "type": "array", "items": { "type": "string" } },
                "why_and_tradeoffs": { "type": "array", "items": { "type": "string" } },
                "latent_insights": { "type": "array", "items": { "type": "string" } },
                "entities": { "type": "array", "items": { "type": "string" } },
                "action_candidates": { "type": "array", "items": { "type": "string" } },
                "pinned_index": { "type": "string" }
            },
            "required": ["narrative"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(narrative) = args
            .get("narrative")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::err("'narrative' must not be empty"));
        };
        let settled_decisions = string_array_arg(&args, "settled_decisions");
        let why_and_tradeoffs = string_array_arg(&args, "why_and_tradeoffs");
        let latent_insights = string_array_arg(&args, "latent_insights");
        let entities = string_array_arg(&args, "entities");
        let inline_candidates = string_array_arg(&args, "action_candidates");
        let pinned_raw = args
            .get("pinned_index")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");

        let result = publish_dream_digests(
            self.memory.as_ref(),
            &self.agent_id,
            &self.day_key,
            &self.window_start,
            &self.window_end,
            narrative,
            settled_decisions,
            why_and_tradeoffs,
            latent_insights,
            entities,
            inline_candidates,
            pinned_raw,
            &self.outcome,
        )
        .await?;

        Ok(ToolResult::ok(result))
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_dream_digests(
    memory: &dyn Memory,
    agent_id: &str,
    day_key: &str,
    window_start: &str,
    window_end: &str,
    narrative: &str,
    settled_decisions: Vec<String>,
    why_and_tradeoffs: Vec<String>,
    latent_insights: Vec<String>,
    entities: Vec<String>,
    inline_candidates: Vec<String>,
    pinned_raw: &str,
    outcome: &SharedOutcome,
) -> Result<serde_json::Value> {
    validate_agent_id(agent_id)?;
    validate_non_soul_key(day_key, "day_key")?;

    let existing = memory
        .load_daily_digests(agent_id, 60)
        .await?
        .into_iter()
        .find(|d| d.day_key == day_key);

    let now_rfc = zeroclaw_api::time::format_rfc3339(jiff::Timestamp::now());

    if let Some(ref existing_digest) = existing
        && existing_digest.owner_edited
    {
        // Machine runs must NEVER overwrite an owner-edited DailyDigest.
        let rendered = render_pinned_index(
            pinned_raw,
            &existing_digest.narrative,
            &existing_digest.settled_decisions,
            &existing_digest.entities,
        );
        let pinned = PinnedDigest {
            agent_id: agent_id.to_string(),
            rendered_index: rendered,
            updated_at: now_rfc,
            source_day_key: Some(day_key.to_string()),
        };
        pinned.validate()?;
        memory.publish_pinned_digest(&pinned).await?;
        let mut guard = outcome.0.lock();
        guard.preserved_owner_digest = true;
        guard.published_digest = Some(existing_digest.clone());
        return Ok(serde_json::json!({
            "daily_digest_preserved_owner_edits": true,
            "day_key": day_key,
            "pinned_refreshed": true,
        }));
    }

    let proposed = outcome.0.lock().proposed_actions.clone();
    let mut action_candidates = Vec::new();
    for c in proposed.into_iter().chain(inline_candidates) {
        let trimmed = c.trim();
        if !trimmed.is_empty() && !action_candidates.iter().any(|x| x == trimmed) {
            action_candidates.push(trimmed.to_string());
        }
    }

    let mut action_decisions = BTreeMap::new();
    if let Some(prev) = existing.as_ref() {
        for (k, v) in &prev.action_decisions {
            if action_candidates.iter().any(|c| c == k) {
                action_decisions.insert(k.clone(), *v);
            }
        }
    }
    for cand in &action_candidates {
        action_decisions
            .entry(cand.clone())
            .or_insert(ActionCandidateStatus::Pending);
    }

    let digest = DailyDigest {
        day_key: day_key.to_string(),
        agent_id: agent_id.to_string(),
        window_start: window_start.to_string(),
        window_end: window_end.to_string(),
        narrative: narrative.to_string(),
        settled_decisions: settled_decisions.clone(),
        why_and_tradeoffs,
        latent_insights,
        action_candidates,
        entities: entities.clone(),
        created_at: now_rfc.clone(),
        owner_edited: false,
        action_decisions,
    };
    digest.validate()?;
    let daily_id = memory.publish_daily_digest(&digest).await?;

    let rendered = render_pinned_index(pinned_raw, narrative, &settled_decisions, &entities);
    let pinned = PinnedDigest {
        agent_id: agent_id.to_string(),
        rendered_index: rendered,
        updated_at: now_rfc,
        source_day_key: Some(day_key.to_string()),
    };
    pinned.validate()?;
    let pinned_id = memory.publish_pinned_digest(&pinned).await?;

    outcome.0.lock().published_digest = Some(digest);
    Ok(serde_json::json!({
        "daily_digest_id": daily_id,
        "pinned_digest_id": pinned_id,
        "day_key": day_key,
        "daily_digest_preserved_owner_edits": false,
    }))
}

fn render_pinned_index(
    pinned_raw: &str,
    narrative: &str,
    settled_decisions: &[String],
    entities: &[String],
) -> String {
    let base = if !pinned_raw.trim().is_empty() {
        pinned_raw.trim().to_string()
    } else {
        let mut lines = Vec::new();
        lines.push(format!("Summary: {}", narrative.trim()));
        if !settled_decisions.is_empty() {
            lines.push(format!("Decisions: {}", settled_decisions.join("; ")));
        }
        if !entities.is_empty() {
            lines.push(format!("Entities: {}", entities.join(", ")));
        }
        lines.join("\n")
    };
    if base.len() <= PINNED_DIGEST_MAX_BYTES {
        base
    } else {
        let suffix = "…";
        let max_prefix = PINNED_DIGEST_MAX_BYTES.saturating_sub(suffix.len());
        let cut = floor_char_boundary(&base, max_prefix);
        format!("{}{suffix}", &base[..cut])
    }
}

async fn run_confined_loop(
    provider: &dyn ModelProvider,
    provider_name: &str,
    model: &str,
    temperature: Option<f64>,
    agent_alias: &str,
    tools: Vec<Box<dyn Tool>>,
    system_prompt: &str,
    user_prompt: &str,
    max_iterations: usize,
) -> Result<String> {
    let pacing = PacingConfig::default();
    let multimodal = MultimodalConfig::default();
    let knobs = LoopKnobs::default();
    let observer = NoopObserver;
    let mut history = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ];
    let turn_id = format!("sleep_{}", uuid::Uuid::new_v4());
    run_tool_call_loop(ToolLoop {
        exec: ResolvedAgentExecution::resolve(
            ResolvedModelAccess {
                model_provider: provider,
                provider_name,
                model,
                temperature,
            },
            ResolvedIo {
                tools_registry: &tools,
                observer: &observer,
                silent: true,
                approval: None,
                multimodal_config: &multimodal,
                config: None,
                hooks: None,
                activated_tools: None,
                receipt_generator: None,
            },
            ResolvedRuntimeKnobs {
                max_tool_iterations: max_iterations,
                excluded_tools: &[],
                dedup_exempt_tools: &[],
                pacing: &pacing,
                strict_tool_parsing: false,
                parallel_tools: false,
                max_tool_result_chars: 16_000,
                keep_tool_context_turns: usize::MAX,
                context_token_budget: 0,
                knobs: &knobs,
            },
        ),
        history: &mut history,
        channel_name: "sleep_compactor",
        channel_reply_target: None,
        cancellation_token: None,
        on_delta: None,
        shared_budget: None,
        channel: None,
        collected_receipts: None,
        event_tx: None,
        steering: None,
        new_messages_out: None,
        image_cache: None,
        memory: None,
        ingress: zeroclaw_api::ingress::IngressContext::sub_turn(),
        agent_alias: Some(agent_alias),
        parent_agent_alias: None,
        turn_id: &turn_id,
    })
    .await
}

// ── Consolidation Turn Parity Path ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TurnParityJson {
    #[serde(default)]
    history_entry: String,
    #[serde(default)]
    memory_update: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default)]
    why_and_tradeoffs: Vec<String>,
    #[serde(default)]
    settled_decisions: Vec<String>,
    #[serde(default)]
    rejected_options: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
}

fn parse_turn_parity_json(raw: &str, fallback_text: &str) -> TurnParityJson {
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(cleaned).unwrap_or_else(|_| TurnParityJson {
        history_entry: truncate_utf8(fallback_text, 200),
        memory_update: None,
        kind: None,
        facts: Vec::new(),
        why_and_tradeoffs: Vec::new(),
        settled_decisions: Vec::new(),
        rejected_options: Vec::new(),
        entities: Vec::new(),
    })
}

/// Confined sleep-compactor replacement for legacy `consolidation::consolidate_turn`.
///
/// Extracts an [`EpisodeRecord`] (when the `Memory` backend supports sleep writes;
/// otherwise writes a `MemoryCategory::Daily` fallback) and Core typed facts
/// honoring `memory_config.types.enabled`, `memory_config.consolidation_extract_facts`,
/// `PolicyEnforcer`, `dedup_gate`, `merge_into_survivor`, and `conflict_supersede_enabled`.
#[allow(clippy::too_many_arguments)]
pub async fn consolidate_turn_via_sleep(
    model_provider: &dyn ModelProvider,
    model: &str,
    temperature: Option<f64>,
    memory: Arc<dyn Memory>,
    memory_config: &MemoryConfig,
    agent_id: &str,
    session_key: &str,
    user_message: &str,
    assistant_response: &str,
) -> Result<()> {
    let turn_text = format!(
        "User: {}\nAssistant: {}",
        strip_media_markers(user_message),
        strip_media_markers(assistant_response),
    );
    let truncated = truncate_utf8(&turn_text, 4000);
    let now_rfc = zeroclaw_api::time::format_rfc3339(jiff::Timestamp::now());
    let outcome = SharedOutcome::new();

    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(EpisodeReadTool {
            memory: Arc::clone(&memory),
            agent_id: agent_id.to_string(),
        }),
        Box::new(EpisodeCommitTool {
            memory: Arc::clone(&memory),
            memory_config: memory_config.clone(),
            agent_id: agent_id.to_string(),
            default_session_key: session_key.to_string(),
            source_kind: EpisodeSourceKind::Dialogue,
            default_started_at: now_rfc.clone(),
            default_ended_at: now_rfc.clone(),
            default_up_to_message_id: None,
            tolerate_unsupported_sleep: true,
            outcome: outcome.clone(),
        }),
        Box::new(MemorySupersedeTool {
            memory: Arc::clone(&memory),
            agent_id: agent_id.to_string(),
            outcome: outcome.clone(),
        }),
    ];

    let final_text = if model_provider.supports_native_tools() {
        run_confined_loop(
            model_provider,
            model_provider.alias(),
            model,
            temperature,
            agent_id,
            tools,
            TURN_PARITY_SYSTEM_PROMPT,
            &truncated,
            4,
        )
        .await?
    } else {
        ProviderDispatch::from_ref(model_provider)
            .chat_with_system(
                Some(TURN_PARITY_SYSTEM_PROMPT),
                &truncated,
                model,
                temperature,
            )
            .await?
    };

    if !outcome.0.lock().committed_episodes.is_empty() {
        return Ok(());
    }

    // Structured JSON / prose fallback when the provider responded without tool calls.
    let parsed = parse_turn_parity_json(&final_text, &turn_text);
    let summary = if parsed.history_entry.trim().is_empty() {
        truncate_utf8(&turn_text, 200)
    } else {
        parsed.history_entry.trim().to_string()
    };
    let episode = EpisodeRecord {
        episode_id: format!("ep_{}", uuid::Uuid::new_v4()),
        agent_id: agent_id.to_string(),
        session_key: session_key.to_string(),
        started_at: now_rfc.clone(),
        ended_at: now_rfc,
        up_to_message_id: None,
        summary: summary.clone(),
        why_and_tradeoffs: parsed.why_and_tradeoffs,
        settled_decisions: parsed.settled_decisions,
        rejected_options: parsed.rejected_options,
        entities: parsed.entities,
        source_kind: EpisodeSourceKind::Dialogue,
    };
    episode.validate()?;

    match memory.commit_episode(&episode).await {
        Ok(_) => {}
        Err(err) if is_unsupported_sleep_error(&err) => {
            let date = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
            let key = format!("daily_{date}_{}", uuid::Uuid::new_v4());
            memory
                .store(&key, &summary, MemoryCategory::Daily, None)
                .await?;
        }
        Err(err) => return Err(err),
    }

    let facts: Vec<ExtractedCoreFact> = parsed
        .facts
        .into_iter()
        .map(|content| ExtractedCoreFact {
            content,
            kind: Some("fact".to_string()),
            key: None,
        })
        .collect();

    persist_extracted_core_facts(
        memory.as_ref(),
        memory_config,
        parsed.memory_update.as_deref(),
        parsed.kind.as_deref(),
        &facts,
    )
    .await
}

// ── Watermark Session Compaction (~65% stage / ~90% or cold-window promote) ─

/// Evaluate token utilization & cold-window age for a live WebSocket session,
/// staging an L1 checkpoint at `>= 65%` and promoting at `>= 90%` or cold-window gap.
#[allow(clippy::too_many_arguments)]
pub async fn maybe_compact_ws_turn(
    config: &Config,
    session_backend: Option<&Arc<dyn SessionBackend>>,
    memory: Option<&Arc<dyn Memory>>,
    agent_alias: &str,
    session_key: &str,
    live_provider_ref: &str,
    live_model: &str,
    context_token_budget: usize,
    reported_input_tokens: Option<u64>,
) -> Result<()> {
    let Some(backend) = session_backend else {
        return Ok(());
    };

    let backend_clone = Arc::clone(backend);
    let key_owned = session_key.to_string();
    let window = tokio::task::spawn_blocking(move || backend_clone.load_session_window(&key_owned))
        .await
        .context("load_session_window task failed")??;

    // 1. Cold-window promotion: if a staged checkpoint has sat through a cold gap,
    //    promote it immediately before inspecting current delta utilization.
    let now_ts = jiff::Timestamp::now();
    if let Some(ref staged) = window.staged_checkpoint {
        let age_secs = now_ts
            .as_second()
            .saturating_sub(staged.updated_at.as_second());
        if age_secs >= COLD_WINDOW_GAP_SECS {
            let b = Arc::clone(backend);
            let k = session_key.to_string();
            let _ = tokio::task::spawn_blocking(move || b.promote_staged_checkpoint(&k)).await;
        }
    }

    if context_token_budget == 0 || window.delta_messages.is_empty() {
        return Ok(());
    }

    let delta_msgs: Vec<ChatMessage> = window
        .delta_messages
        .iter()
        .map(|(_, tm)| tm.message.clone())
        .collect();
    let estimated_tokens = estimate_history_tokens(&delta_msgs) as u64;
    let used_tokens = reported_input_tokens.unwrap_or(estimated_tokens);
    let budget = context_token_budget as u64;

    let stage_due =
        used_tokens.saturating_mul(100) >= budget.saturating_mul(WATERMARK_STAGE_PERCENT as u64);
    let promote_due =
        used_tokens.saturating_mul(100) >= budget.saturating_mul(WATERMARK_PROMOTE_PERCENT as u64);

    if !stage_due {
        return Ok(());
    }

    let max_delta_id = window
        .delta_messages
        .iter()
        .map(|(id, _)| *id)
        .max()
        .unwrap_or(0);
    if max_delta_id <= 0 {
        return Ok(());
    }

    let already_staged_current = window
        .staged_checkpoint
        .as_ref()
        .is_some_and(|c| c.up_to_message_id >= max_delta_id);

    if !already_staged_current {
        let Some(sleep_model) = resolve_fast_sleep_model(config, live_provider_ref, live_model)
        else {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "model_provider": live_provider_ref,
                        "agent": agent_alias,
                    })),
                "WS sleep compaction skipped: provider no longer resolves"
            );
            return Ok(());
        };

        let transcript = format_delta_transcript(&window.delta_messages);
        let started_at = window
            .delta_messages
            .first()
            .and_then(|(_, tm)| tm.created_at)
            .unwrap_or(now_ts);
        let ended_at = window
            .delta_messages
            .last()
            .and_then(|(_, tm)| tm.created_at)
            .unwrap_or(now_ts)
            .max(started_at);

        let started_rfc = zeroclaw_api::time::format_rfc3339(started_at);
        let ended_rfc = zeroclaw_api::time::format_rfc3339(ended_at);
        let outcome = SharedOutcome::new();

        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(SessionSearchTool {
                session_backend: Arc::clone(backend),
                allowed_session_keys: vec![session_key.to_string()],
            }),
            Box::new(CheckpointStageTool {
                session_backend: Arc::clone(backend),
                session_key: session_key.to_string(),
                max_message_id: max_delta_id,
                outcome: outcome.clone(),
            }),
        ];
        if let Some(mem) = memory {
            tools.push(Box::new(EpisodeReadTool {
                memory: Arc::clone(mem),
                agent_id: agent_alias.to_string(),
            }));
            tools.push(Box::new(EpisodeCommitTool {
                memory: Arc::clone(mem),
                memory_config: config.memory.clone(),
                agent_id: agent_alias.to_string(),
                default_session_key: session_key.to_string(),
                source_kind: EpisodeSourceKind::Dialogue,
                default_started_at: started_rfc.clone(),
                default_ended_at: ended_rfc.clone(),
                default_up_to_message_id: Some(max_delta_id),
                tolerate_unsupported_sleep: true,
                outcome: outcome.clone(),
            }));
            tools.push(Box::new(MemorySupersedeTool {
                memory: Arc::clone(mem),
                agent_id: agent_alias.to_string(),
                outcome: outcome.clone(),
            }));
        }

        let user_prompt = format!(
            "Session `{session_key}` reached {used_tokens}/{budget} tokens (watermark up_to_message_id={max_delta_id}).\n\nTranscript:\n{transcript}"
        );
        let final_text = run_confined_loop(
            sleep_model.provider.as_ref(),
            &sleep_model.provider_name,
            &sleep_model.model,
            sleep_model.temperature,
            agent_alias,
            tools,
            WATERMARK_COMPACTOR_SYSTEM_PROMPT,
            &user_prompt,
            6,
        )
        .await?;

        // Fallback if the model answered with plain text / JSON without calling `checkpoint_stage`.
        if outcome.0.lock().staged_checkpoint.is_none() {
            let fallback_digest = build_fallback_digest(&final_text, &transcript);
            let b = Arc::clone(backend);
            let k = session_key.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                b.stage_checkpoint(&k, max_delta_id, &fallback_digest)
            })
            .await;
        }
    }

    if promote_due {
        let b = Arc::clone(backend);
        let k = session_key.to_string();
        let _ = tokio::task::spawn_blocking(move || b.promote_staged_checkpoint(&k)).await;
    }

    Ok(())
}

fn format_delta_transcript(
    delta_messages: &[(i64, zeroclaw_infra::session_backend::TimestampedMessage)],
) -> String {
    let mut out = String::new();
    for (id, tm) in delta_messages {
        let cleaned = strip_media_markers(&tm.message.content);
        let line = format!("[msg:{id}] {}: {}\n", tm.message.role, cleaned);
        if out.len() + line.len() > 16_000 {
            out.push_str("[... earlier/later messages truncated for sleep budget ...]\n");
            break;
        }
        out.push_str(&line);
    }
    out
}

fn build_fallback_digest(final_text: &str, transcript: &str) -> String {
    let trimmed = final_text.trim();
    if !trimmed.is_empty() {
        truncate_utf8(trimmed, 4_000)
    } else {
        truncate_utf8(transcript, 1_500)
    }
}

// ── Quiet Hours & 04:00 Day Cut Scheduling ──────────────────────────────────

/// Return `true` when `now` falls inside configured `AttentionConfig` quiet hours.
pub fn is_quiet_hours(policy: Option<&AttentionConfig>, now: jiff::Timestamp) -> bool {
    let Some(policy) = policy else {
        return false;
    };
    let Ok(timezone) = jiff::tz::TimeZone::get(&policy.timezone) else {
        return true;
    };
    let Ok(start) = jiff::civil::Time::strptime("%H:%M", &policy.quiet_start) else {
        return true;
    };
    let Ok(end) = jiff::civil::Time::strptime("%H:%M", &policy.quiet_end) else {
        return true;
    };
    let local = now.to_zoned(timezone).time();
    if start < end {
        local >= start && local < end
    } else {
        local >= start || local < end
    }
}

/// Resolved local `04:00` Auto Dream window for a given instant and timezone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamWindow {
    pub day_key: String,
    pub window_start: String,
    pub window_end: String,
}

/// Compute the most recently completed local `04:00` day-cut window for `now`.
pub fn due_dream_window(now: jiff::Timestamp, timezone: jiff::tz::TimeZone) -> Result<DreamWindow> {
    let zoned = now.to_zoned(timezone.clone());
    let cut_time = jiff::civil::time(DREAM_CUT_HOUR, 0, 0, 0);
    let end_date = if zoned.time() >= cut_time {
        zoned.date()
    } else {
        zoned.date().yesterday()?
    };
    let start_date = end_date.yesterday()?;
    let window_end_zoned = timezone.to_zoned(end_date.at(DREAM_CUT_HOUR, 0, 0, 0))?;
    let window_start_zoned = timezone.to_zoned(start_date.at(DREAM_CUT_HOUR, 0, 0, 0))?;
    Ok(DreamWindow {
        day_key: end_date.strftime("%Y-%m-%d").to_string(),
        window_start: zeroclaw_api::time::format_rfc3339(window_start_zoned.timestamp()),
        window_end: zeroclaw_api::time::format_rfc3339(window_end_zoned.timestamp()),
    })
}

fn resolve_dream_timezone(config: &Config) -> jiff::tz::TimeZone {
    if let Some(tz_name) = config
        .gateway
        .attention
        .as_ref()
        .map(|a| a.timezone.trim())
        .filter(|s| !s.is_empty())
        && let Ok(tz) = jiff::tz::TimeZone::get(tz_name)
    {
        return tz;
    }
    jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DreamStateFile {
    #[serde(default)]
    last_day_key_by_agent: BTreeMap<String, String>,
}

fn dream_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DREAM_STATE_FILENAME)
}

fn load_dream_state(data_dir: &Path) -> DreamStateFile {
    let path = dream_state_path(data_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return DreamStateFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_dream_state(data_dir: &Path, state: &DreamStateFile) -> Result<()> {
    let path = dream_state_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = data_dir.join(format!(
        "{DREAM_STATE_FILENAME}.tmp.{}",
        uuid::Uuid::new_v4()
    ));
    let encoded = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp, encoded)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ── Daemon Worker: Startup Recovery, Idle Flush, Light Sleep & 04:00 Dream ──

fn agent_uses_tachi(config: &Config, agent_alias: &str) -> bool {
    let Some(agent_cfg) = config.agent(agent_alias) else {
        return false;
    };
    if !agent_cfg.enabled {
        return false;
    }
    if matches!(
        agent_cfg.memory.backend,
        MemoryBackendKind::None | MemoryBackendKind::Markdown
    ) {
        return false;
    }
    true
}

fn session_belongs_to_agent(meta: &SessionMetadata, agent_alias: &str, single_agent: bool) -> bool {
    match meta.agent_alias.as_deref() {
        Some(owner) => owner == agent_alias,
        None => single_agent,
    }
}

/// Flush any unflushed turns in one session into an `EpisodeRecord` (and promote
/// any staged checkpoint). Used on daemon startup (`StartupRecovery`) and idle
/// session detection (`IdleFlush`).
pub async fn flush_session_to_episode(
    config: &Config,
    backend: &Arc<dyn SessionBackend>,
    memory: &Arc<dyn Memory>,
    agent_alias: &str,
    session_key: &str,
    source_kind: EpisodeSourceKind,
    sleep_model: Option<&ResolvedSleepModel>,
) -> Result<bool> {
    let b = Arc::clone(backend);
    let k = session_key.to_string();
    let window = tokio::task::spawn_blocking(move || b.load_session_window(&k))
        .await
        .context("load_session_window failed")??;

    if window.delta_messages.is_empty() {
        if window.staged_checkpoint.is_some() {
            let b = Arc::clone(backend);
            let k = session_key.to_string();
            let _ = tokio::task::spawn_blocking(move || b.promote_staged_checkpoint(&k)).await;
        }
        return Ok(false);
    }

    let max_delta_id = window
        .delta_messages
        .iter()
        .map(|(id, _)| *id)
        .max()
        .unwrap_or(0);
    if max_delta_id <= 0 {
        return Ok(false);
    }

    let existing = memory
        .read_episodes_in_window(agent_alias, None, None, Some(session_key), 1)
        .await?;
    if let Some(latest) = existing.first()
        && latest.up_to_message_id.is_some_and(|id| id >= max_delta_id)
    {
        return Ok(false);
    }

    let now_ts = jiff::Timestamp::now();
    let started_at = window
        .delta_messages
        .first()
        .and_then(|(_, tm)| tm.created_at)
        .unwrap_or(now_ts);
    let ended_at = window
        .delta_messages
        .last()
        .and_then(|(_, tm)| tm.created_at)
        .unwrap_or(now_ts)
        .max(started_at);
    let started_rfc = zeroclaw_api::time::format_rfc3339(started_at);
    let ended_rfc = zeroclaw_api::time::format_rfc3339(ended_at);

    let transcript = format_delta_transcript(&window.delta_messages);
    let outcome = SharedOutcome::new();

    if let Some(model) = sleep_model {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(SessionSearchTool {
                session_backend: Arc::clone(backend),
                allowed_session_keys: vec![session_key.to_string()],
            }),
            Box::new(EpisodeReadTool {
                memory: Arc::clone(memory),
                agent_id: agent_alias.to_string(),
            }),
            Box::new(EpisodeCommitTool {
                memory: Arc::clone(memory),
                memory_config: config.memory.clone(),
                agent_id: agent_alias.to_string(),
                default_session_key: session_key.to_string(),
                source_kind,
                default_started_at: started_rfc.clone(),
                default_ended_at: ended_rfc.clone(),
                default_up_to_message_id: Some(max_delta_id),
                tolerate_unsupported_sleep: false,
                outcome: outcome.clone(),
            }),
            Box::new(MemorySupersedeTool {
                memory: Arc::clone(memory),
                agent_id: agent_alias.to_string(),
                outcome: outcome.clone(),
            }),
        ];
        let prompt = format!(
            "Flush unflushed session `{session_key}` ({source_kind}) up to message_id={max_delta_id}.\n\nTranscript:\n{transcript}"
        );
        let final_text = run_confined_loop(
            model.provider.as_ref(),
            &model.provider_name,
            &model.model,
            model.temperature,
            agent_alias,
            tools,
            EPISODE_FLUSH_SYSTEM_PROMPT,
            &prompt,
            5,
        )
        .await?;

        if outcome.0.lock().committed_episodes.is_empty() {
            let episode = EpisodeRecord {
                episode_id: format!("ep_{session_key}_{max_delta_id}"),
                agent_id: agent_alias.to_string(),
                session_key: session_key.to_string(),
                started_at: started_rfc,
                ended_at: ended_rfc,
                up_to_message_id: Some(max_delta_id),
                summary: build_fallback_digest(&final_text, &transcript),
                why_and_tradeoffs: Vec::new(),
                settled_decisions: Vec::new(),
                rejected_options: Vec::new(),
                entities: Vec::new(),
                source_kind,
            };
            episode.validate()?;
            memory.commit_episode(&episode).await?;
        }
    } else {
        // Deterministic recovery fallback when no provider resolves cleanly.
        let episode = EpisodeRecord {
            episode_id: format!("ep_{session_key}_{max_delta_id}"),
            agent_id: agent_alias.to_string(),
            session_key: session_key.to_string(),
            started_at: started_rfc,
            ended_at: ended_rfc,
            up_to_message_id: Some(max_delta_id),
            summary: truncate_utf8(&transcript, 1_500),
            why_and_tradeoffs: Vec::new(),
            settled_decisions: Vec::new(),
            rejected_options: Vec::new(),
            entities: Vec::new(),
            source_kind,
        };
        episode.validate()?;
        memory.commit_episode(&episode).await?;
    }

    if window.staged_checkpoint.is_some() {
        let b = Arc::clone(backend);
        let k = session_key.to_string();
        let _ = tokio::task::spawn_blocking(move || b.promote_staged_checkpoint(&k)).await;
    }

    Ok(true)
}

/// Run one `04:00` Deep Auto Dream pass for `agent_alias` over `[window.window_start, window.window_end]`.
///
/// Uses the agent's primary flagship model (`primary_model`), respects existing
/// `owner_edited = true` `DailyDigest`s, updates `PinnedDigest` (`<= 8_000` bytes),
/// and respects quiet hours for any optional summary notification.
pub async fn run_deep_dream_for_agent(
    config: &Config,
    backend: &Arc<dyn SessionBackend>,
    memory: &Arc<dyn Memory>,
    agent_alias: &str,
    window: &DreamWindow,
    primary_model: &ResolvedSleepModel,
    now: jiff::Timestamp,
) -> Result<bool> {
    let episodes = memory
        .read_episodes_in_window(
            agent_alias,
            Some(&window.window_start),
            Some(&window.window_end),
            None,
            100,
        )
        .await?;

    if episodes.is_empty() {
        return Ok(false);
    }

    let allowed_session_keys: Vec<String> = {
        let b = Arc::clone(backend);
        let alias_owned = agent_alias.to_string();
        let single_agent = config.agents.len() == 1;
        tokio::task::spawn_blocking(move || {
            b.list_sessions_with_metadata()
                .into_iter()
                .filter(|m| session_belongs_to_agent(m, &alias_owned, single_agent))
                .map(|m| m.key)
                .collect()
        })
        .await
        .context("list_sessions_with_metadata failed")?
    };

    let existing_pinned = memory.load_pinned_digest(agent_alias).await?;
    let outcome = SharedOutcome::new();

    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(SessionSearchTool {
            session_backend: Arc::clone(backend),
            allowed_session_keys,
        }),
        Box::new(EpisodeReadTool {
            memory: Arc::clone(memory),
            agent_id: agent_alias.to_string(),
        }),
        Box::new(MemorySupersedeTool {
            memory: Arc::clone(memory),
            agent_id: agent_alias.to_string(),
            outcome: outcome.clone(),
        }),
        Box::new(ActionProposeTool {
            outcome: outcome.clone(),
        }),
        Box::new(DigestPublishTool {
            memory: Arc::clone(memory),
            agent_id: agent_alias.to_string(),
            day_key: window.day_key.clone(),
            window_start: window.window_start.clone(),
            window_end: window.window_end.clone(),
            outcome: outcome.clone(),
        }),
    ];

    let episodes_json = serde_json::to_string_pretty(&episodes)?;
    let pinned_note = existing_pinned
        .as_ref()
        .map(|p| p.rendered_index.as_str())
        .unwrap_or("(none)");
    let prompt = format!(
        "Deep Auto Dream for agent `{agent_alias}`, day_key `{}` (window {} .. {}).\n\nExisting PinnedDigest:\n{}\n\nEpisodes in window:\n{}",
        window.day_key,
        window.window_start,
        window.window_end,
        truncate_utf8(pinned_note, 4_000),
        truncate_utf8(&episodes_json, 24_000),
    );

    let final_text = run_confined_loop(
        primary_model.provider.as_ref(),
        &primary_model.provider_name,
        &primary_model.model,
        primary_model.temperature,
        agent_alias,
        tools,
        DEEP_DREAM_SYSTEM_PROMPT,
        &prompt,
        8,
    )
    .await?;

    if outcome.0.lock().published_digest.is_none() {
        publish_dream_fallback(
            memory.as_ref(),
            agent_alias,
            window,
            &episodes,
            &final_text,
            &outcome,
        )
        .await?;
    }

    let quiet = is_quiet_hours(config.gateway.attention.as_ref(), now);
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_attrs(::serde_json::json!({
                "agent": agent_alias,
                "day_key": window.day_key,
                "episodes": episodes.len(),
                "quiet_hours": quiet,
                "preserved_owner_edits": outcome.0.lock().preserved_owner_digest,
            })),
        if quiet {
            "Auto Dream complete (summary notification suppressed during quiet hours)"
        } else {
            "Auto Dream complete"
        }
    );

    Ok(true)
}

#[derive(Debug, Deserialize)]
struct DreamFallbackJson {
    #[serde(default)]
    narrative: String,
    #[serde(default)]
    settled_decisions: Vec<String>,
    #[serde(default)]
    why_and_tradeoffs: Vec<String>,
    #[serde(default)]
    latent_insights: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    action_candidates: Vec<String>,
    #[serde(default)]
    pinned_index: String,
}

async fn publish_dream_fallback(
    memory: &dyn Memory,
    agent_id: &str,
    window: &DreamWindow,
    episodes: &[EpisodeRecord],
    final_text: &str,
    outcome: &SharedOutcome,
) -> Result<()> {
    let cleaned = final_text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(parsed) = serde_json::from_str::<DreamFallbackJson>(cleaned)
        && !parsed.narrative.trim().is_empty()
    {
        let _ = publish_dream_digests(
            memory,
            agent_id,
            &window.day_key,
            &window.window_start,
            &window.window_end,
            parsed.narrative.trim(),
            parsed.settled_decisions,
            parsed.why_and_tradeoffs,
            parsed.latent_insights,
            parsed.entities,
            parsed.action_candidates,
            &parsed.pinned_index,
            outcome,
        )
        .await?;
        return Ok(());
    }

    let mut settled = Vec::new();
    let mut why = Vec::new();
    let mut entities = Vec::new();
    for ep in episodes {
        for d in &ep.settled_decisions {
            if !settled.iter().any(|x| x == d) {
                settled.push(d.clone());
            }
        }
        for w in &ep.why_and_tradeoffs {
            if !why.iter().any(|x| x == w) {
                why.push(w.clone());
            }
        }
        for e in &ep.entities {
            if !entities.iter().any(|x| x == e) {
                entities.push(e.clone());
            }
        }
    }
    let narrative = if !final_text.trim().is_empty() {
        truncate_utf8(final_text.trim(), 4_000)
    } else {
        episodes
            .iter()
            .map(|e| e.summary.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let _ = publish_dream_digests(
        memory,
        agent_id,
        &window.day_key,
        &window.window_start,
        &window.window_end,
        &narrative,
        settled,
        why,
        Vec::new(),
        entities,
        Vec::new(),
        "",
        outcome,
    )
    .await?;
    Ok(())
}

async fn tick_agent(
    config: &Config,
    backend: &Arc<dyn SessionBackend>,
    dream_state: &mut DreamStateFile,
    agent_alias: &str,
    startup_pass: bool,
    now: jiff::Timestamp,
) -> Result<bool> {
    if !agent_uses_tachi(config, agent_alias) {
        return Ok(false);
    }

    let api_key = config
        .resolved_model_provider_for_agent(agent_alias)
        .and_then(|(_, _, cfg)| cfg.api_key.clone());
    let memory =
        zeroclaw_memory::create_memory_for_agent(config, agent_alias, api_key.as_deref()).await?;

    let b = Arc::clone(backend);
    let sessions = tokio::task::spawn_blocking(move || b.list_sessions_with_metadata())
        .await
        .context("list_sessions_with_metadata failed")?;

    let single_agent = config.agents.len() == 1;
    let fast_model = resolve_daemon_fast_sleep_model(config, agent_alias);

    for meta in sessions {
        if !session_belongs_to_agent(&meta, agent_alias, single_agent) {
            continue;
        }
        let b_state = Arc::clone(backend);
        let key_owned = meta.key.clone();
        let state = tokio::task::spawn_blocking(move || b_state.get_session_state(&key_owned))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten();
        if state.as_ref().is_some_and(|s| s.state == "running") {
            continue;
        }

        let idle_secs = now
            .as_second()
            .saturating_sub(meta.last_activity.as_second());
        let (should_flush, source_kind) = if startup_pass {
            (true, EpisodeSourceKind::StartupRecovery)
        } else if idle_secs >= COLD_WINDOW_GAP_SECS {
            (true, EpisodeSourceKind::IdleFlush)
        } else {
            (false, EpisodeSourceKind::IdleFlush)
        };

        if should_flush
            && let Err(err) = flush_session_to_episode(
                config,
                backend,
                &memory,
                agent_alias,
                &meta.key,
                source_kind,
                fast_model.as_ref(),
            )
            .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "agent": agent_alias,
                        "session_key": meta.key,
                        "error": err.to_string(),
                    })),
                "sleep episode flush failed"
            );
        }
    }

    if let Err(err) = memory.run_light_sleep_governance() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "agent": agent_alias,
                    "error": err.to_string(),
                })),
            "light sleep governance failed"
        );
    }

    let tz = resolve_dream_timezone(config);
    let window = due_dream_window(now, tz)?;
    let last_done = dream_state.last_day_key_by_agent.get(agent_alias);
    let dream_due = last_done.is_none_or(|prev| prev.as_str() < window.day_key.as_str());
    let mut state_dirty = false;
    if dream_due && let Some(primary_model) = resolve_primary_sleep_model(config, agent_alias) {
        run_deep_dream_for_agent(
            config,
            backend,
            &memory,
            agent_alias,
            &window,
            &primary_model,
            now,
        )
        .await?;
        dream_state
            .last_day_key_by_agent
            .insert(agent_alias.to_string(), window.day_key);
        state_dirty = true;
    }

    Ok(state_dirty)
}

/// Daemon worker: handles startup session recovery, periodic idle session
/// flushes, light-sleep governance, and the local `04:00` Auto Dream (including
/// missed-window catch-up via a lightweight JSON marker in `config.data_dir`).
pub async fn run_worker(config: Config, cancel: CancellationToken) -> Result<()> {
    let mut interval = tokio::time::interval(SLEEP_CHECK_INTERVAL);
    let mut startup_pass = true;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if config.agents.is_empty() || !config.data_dir.is_dir() {
            startup_pass = false;
            continue;
        }
        let backend = zeroclaw_infra::make_session_backend(
            &config.data_dir,
            &config.channels.session_backend,
        )?;
        let mut dream_state = load_dream_state(&config.data_dir);
        let mut state_dirty = false;

        let mut aliases: Vec<String> = config.agents.keys().cloned().collect();
        aliases.sort();
        let now = jiff::Timestamp::now();
        for alias in &aliases {
            match tick_agent(
                &config,
                &backend,
                &mut dream_state,
                alias,
                startup_pass,
                now,
            )
            .await
            {
                Ok(dirty) => state_dirty |= dirty,
                Err(err) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "agent": alias,
                                "error": err.to_string(),
                            })),
                        "sleep compactor worker tick failed"
                    );
                }
            }
        }
        if state_dirty && let Err(err) = save_dream_state(&config.data_dir, &dream_state) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({ "error": err.to_string() })),
                "failed to persist sleep_dream_state.json"
            );
        }
        startup_pass = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use zeroclaw_api::attribution::{
        Attributable, MemoryKind as AttrMemoryKind, ModelProviderKind, ProviderKind, Role,
    };
    use zeroclaw_api::model_provider::{ChatRequest, ChatResponse, ProviderCapabilities, ToolCall};
    use zeroclaw_config::multi_agent::AgentMemoryConfig;
    use zeroclaw_config::schema::{
        AliasedAgentConfig, ModelProviderConfig, ModelRouteConfig, OllamaModelProviderConfig,
        OpenAIModelProviderConfig,
    };
    use zeroclaw_infra::session_sqlite::SqliteSessionBackend;

    #[derive(Default)]
    struct InMemorySleepStore {
        entries: Mutex<Vec<MemoryEntry>>,
        episodes: Mutex<Vec<EpisodeRecord>>,
        pinned: Mutex<BTreeMap<String, PinnedDigest>>,
        digests: Mutex<Vec<DailyDigest>>,
    }

    impl Attributable for InMemorySleepStore {
        fn role(&self) -> Role {
            Role::Memory(AttrMemoryKind::InMemory)
        }
        fn alias(&self) -> &str {
            "in-memory-sleep"
        }
    }

    #[async_trait]
    impl Memory for InMemorySleepStore {
        fn name(&self) -> &str {
            "tachi"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.store_with_options(key, content, category, session_id, StoreOptions::default())
                .await
        }

        async fn store_with_options(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
            options: StoreOptions,
        ) -> anyhow::Result<()> {
            let mut guard = self.entries.lock();
            if let Some(existing) = guard.iter_mut().find(|e| e.key == key) {
                existing.content = content.to_string();
                existing.category = category;
                existing.kind = options.kind;
                existing.importance = options.importance;
                return Ok(());
            }
            guard.push(MemoryEntry {
                id: format!("id_{key}"),
                key: key.to_string(),
                content: content.to_string(),
                category,
                timestamp: zeroclaw_api::time::format_rfc3339(jiff::Timestamp::now()),
                session_id: session_id.map(str::to_string),
                score: Some(1.0),
                namespace: options.namespace.unwrap_or_else(|| "default".into()),
                importance: options.importance,
                superseded_by: None,
                kind: options.kind,
                pinned: options.pinned,
                tenant_id: options.tenant_id,
                agent_alias: None,
                agent_id: None,
            });
            Ok(())
        }

        async fn store_with_agent(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
            namespace: Option<&str>,
            importance: Option<f64>,
            agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            let mut guard = self.entries.lock();
            guard.push(MemoryEntry {
                id: format!("id_{key}"),
                key: key.to_string(),
                content: content.to_string(),
                category,
                timestamp: zeroclaw_api::time::format_rfc3339(jiff::Timestamp::now()),
                session_id: session_id.map(str::to_string),
                score: Some(1.0),
                namespace: namespace.unwrap_or("default").to_string(),
                importance,
                superseded_by: None,
                kind: None,
                pinned: false,
                tenant_id: None,
                agent_alias: agent_id.map(str::to_string),
                agent_id: agent_id.map(str::to_string),
            });
            Ok(())
        }

        async fn recall(
            &self,
            query: &str,
            limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            let q = query.to_ascii_lowercase();
            let guard = self.entries.lock();
            Ok(guard
                .iter()
                .filter(|e| e.superseded_by.is_none())
                .filter(|e| q.is_empty() || e.content.to_ascii_lowercase().contains(&q))
                .take(limit)
                .cloned()
                .collect())
        }

        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.recall(query, limit, session_id, since, until).await
        }

        async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(self.entries.lock().iter().find(|e| e.key == key).cloned())
        }

        async fn list(
            &self,
            category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self
                .entries
                .lock()
                .iter()
                .filter(|e| category.is_none_or(|c| &e.category == c))
                .cloned()
                .collect())
        }

        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            let mut guard = self.entries.lock();
            let before = guard.len();
            guard.retain(|e| e.key != key);
            Ok(guard.len() < before)
        }

        async fn forget_for_agent(&self, key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            self.forget(key).await
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.entries.lock().len())
        }

        async fn health_check(&self) -> bool {
            true
        }

        async fn supersede(&self, ids: &[String], replacement_id: &str) -> anyhow::Result<()> {
            let mut guard = self.entries.lock();
            for entry in guard.iter_mut() {
                if ids.iter().any(|id| id == &entry.id) {
                    entry.superseded_by = Some(replacement_id.to_string());
                }
            }
            Ok(())
        }

        async fn commit_episode(&self, episode: &EpisodeRecord) -> anyhow::Result<String> {
            episode.validate()?;
            let mut guard = self.episodes.lock();
            if let Some(existing) = guard
                .iter_mut()
                .find(|e| e.agent_id == episode.agent_id && e.episode_id == episode.episode_id)
            {
                *existing = episode.clone();
            } else {
                guard.push(episode.clone());
            }
            Ok(episode.episode_id.clone())
        }

        async fn read_episodes_in_window(
            &self,
            agent_id: &str,
            since: Option<&str>,
            until: Option<&str>,
            session_id: Option<&str>,
            limit: usize,
        ) -> anyhow::Result<Vec<EpisodeRecord>> {
            validate_agent_id(agent_id)?;
            let since_ts = since.map(zeroclaw_api::time::parse_rfc3339).transpose()?;
            let until_ts = until.map(zeroclaw_api::time::parse_rfc3339).transpose()?;
            let mut out: Vec<EpisodeRecord> = self
                .episodes
                .lock()
                .iter()
                .filter(|e| e.agent_id == agent_id)
                .filter(|e| session_id.is_none_or(|sid| e.session_key == sid))
                .filter(|e| {
                    let Ok(end) = zeroclaw_api::time::parse_rfc3339(&e.ended_at) else {
                        return false;
                    };
                    since_ts.is_none_or(|s| end >= s) && until_ts.is_none_or(|u| end <= u)
                })
                .cloned()
                .collect();
            out.reverse();
            out.truncate(limit);
            Ok(out)
        }

        async fn supersede_fact(
            &self,
            agent_id: &str,
            old_key: &str,
            new_key: &str,
            new_content: &str,
            namespace: Option<&str>,
        ) -> anyhow::Result<String> {
            validate_agent_id(agent_id)?;
            validate_non_soul_key(old_key, "old_key")?;
            validate_non_soul_key(new_key, "new_key")?;
            validate_non_soul_namespace(namespace)?;
            let mut guard = self.entries.lock();
            let old = guard
                .iter_mut()
                .find(|e| e.key == old_key && e.agent_id.as_deref() == Some(agent_id))
                .ok_or_else(|| anyhow::Error::msg("old_key not found"))?;
            let new_id = format!("id_{new_key}");
            old.superseded_by = Some(new_id.clone());
            guard.push(MemoryEntry {
                id: new_id.clone(),
                key: new_key.to_string(),
                content: new_content.to_string(),
                category: MemoryCategory::Core,
                timestamp: zeroclaw_api::time::format_rfc3339(jiff::Timestamp::now()),
                session_id: None,
                score: Some(1.0),
                namespace: namespace.unwrap_or("default").to_string(),
                importance: Some(0.7),
                superseded_by: None,
                kind: None,
                pinned: false,
                tenant_id: None,
                agent_alias: Some(agent_id.to_string()),
                agent_id: Some(agent_id.to_string()),
            });
            Ok(new_id)
        }

        async fn publish_pinned_digest(&self, digest: &PinnedDigest) -> anyhow::Result<String> {
            digest.validate()?;
            self.pinned
                .lock()
                .insert(digest.agent_id.clone(), digest.clone());
            Ok(format!("pinned:{}", digest.agent_id))
        }

        async fn load_pinned_digest(&self, agent_id: &str) -> anyhow::Result<Option<PinnedDigest>> {
            validate_agent_id(agent_id)?;
            Ok(self.pinned.lock().get(agent_id).cloned())
        }

        async fn publish_daily_digest(&self, digest: &DailyDigest) -> anyhow::Result<String> {
            digest.validate()?;
            let mut guard = self.digests.lock();
            if let Some(existing) = guard
                .iter_mut()
                .find(|d| d.agent_id == digest.agent_id && d.day_key == digest.day_key)
            {
                *existing = digest.clone();
            } else {
                guard.push(digest.clone());
            }
            Ok(format!("daily:{}:{}", digest.agent_id, digest.day_key))
        }

        async fn load_daily_digests(
            &self,
            agent_id: &str,
            limit: usize,
        ) -> anyhow::Result<Vec<DailyDigest>> {
            validate_agent_id(agent_id)?;
            let mut out: Vec<DailyDigest> = self
                .digests
                .lock()
                .iter()
                .filter(|d| d.agent_id == agent_id)
                .cloned()
                .collect();
            out.reverse();
            out.truncate(limit);
            Ok(out)
        }
    }

    struct ScriptedNativeProvider {
        alias: &'static str,
        responses: Mutex<Vec<ChatResponse>>,
        seen_models: Mutex<Vec<String>>,
        seen_tools: Mutex<Vec<Vec<String>>>,
        calls: AtomicUsize,
    }

    impl ScriptedNativeProvider {
        fn new(alias: &'static str, responses: Vec<ChatResponse>) -> Self {
            Self {
                alias,
                responses: Mutex::new(responses),
                seen_models: Mutex::new(Vec::new()),
                seen_tools: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Attributable for ScriptedNativeProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            self.alias
        }
    }

    #[async_trait]
    impl ModelProvider for ScriptedNativeProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            self.seen_models.lock().push(model.to_string());
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("done".to_string())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            self.seen_models.lock().push(model.to_string());
            if let Some(tools) = request.tools {
                self.seen_tools
                    .lock()
                    .push(tools.iter().map(|t| t.name.clone()).collect());
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.responses.lock();
            if guard.is_empty() {
                Ok(ChatResponse {
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    usage: None,
                    reasoning_content: None,
                })
            } else {
                Ok(guard.remove(0))
            }
        }
    }

    struct FailingProvider;

    impl Attributable for FailingProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "failing"
        }
    }

    #[async_trait]
    impl ModelProvider for FailingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            anyhow::bail!("synthetic provider error")
        }
    }

    fn sleep_fixture() -> (TempDir, Arc<InMemorySleepStore>, Arc<dyn SessionBackend>) {
        let dir = TempDir::new().unwrap();
        let mem = Arc::new(InMemorySleepStore::default());
        let sessions: Arc<dyn SessionBackend> =
            Arc::new(SqliteSessionBackend::new(dir.path()).unwrap());
        (dir, mem, sessions)
    }

    #[test]
    fn split_model_routing_uses_hint_fast_when_configured_and_falls_back_to_primary() {
        let mut config = Config::default();
        config.providers.models.openai.insert(
            "flagship".into(),
            OpenAIModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("gpt-5".into()),
                    temperature: Some(0.2),
                    ..Default::default()
                },
            },
        );
        config.providers.models.ollama.insert(
            "fastlocal".into(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("llama3.2".into()),
                    temperature: Some(0.1),
                    ..Default::default()
                },
            },
        );
        config.agents.insert(
            "nova".into(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: "openai.flagship".into(),
                memory: AgentMemoryConfig {
                    backend: MemoryBackendKind::Tachi,
                },
                ..Default::default()
            },
        );

        let fast_fallback = resolve_fast_sleep_model(&config, "openai.flagship", "gpt-5").unwrap();
        assert_eq!(fast_fallback.model, "gpt-5");
        assert_eq!(fast_fallback.temperature, Some(0.2));

        assert!(resolve_fast_sleep_model(&config, "openai.removed", "gpt-5").is_none());

        config.model_routes.push(ModelRouteConfig {
            hint: "fast".into(),
            model_provider: "ollama.fastlocal".into(),
            model: "llama3.2".into(),
            api_key: None,
        });
        let fast_routed = resolve_fast_sleep_model(&config, "openai.flagship", "gpt-5").unwrap();
        assert_eq!(fast_routed.model, "hint:fast");
        assert_eq!(fast_routed.temperature, Some(0.1));

        let primary_dream = resolve_primary_sleep_model(&config, "nova").unwrap();
        assert_eq!(primary_dream.model, "gpt-5");
        assert_eq!(primary_dream.temperature, Some(0.2));
    }

    #[tokio::test]
    async fn confined_tool_loop_exposes_only_sleep_primitives_and_preserves_thoughts() {
        let (_dir, store, sessions) = sleep_fixture();
        let mem: Arc<dyn Memory> = store.clone();
        sessions
            .append(
                "sess_1",
                &ChatMessage::user("Should we use SQLite or Postgres?"),
            )
            .unwrap();
        sessions
            .append(
                "sess_1",
                &ChatMessage::assistant(
                    "SQLite with single handle avoids lock contention; Postgres rejected as overkill.",
                ),
            )
            .unwrap();

        let provider = ScriptedNativeProvider::new(
            "sleep-test",
            vec![
                ChatResponse {
                    text: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "call_ckpt".into(),
                            name: "checkpoint_stage".into(),
                            arguments: serde_json::json!({
                                "up_to_message_id": 2,
                                "digest": "Settled single SQLite handle; rejected Postgres."
                            })
                            .to_string(),
                            extra_content: None,
                        },
                        ToolCall {
                            id: "call_ep".into(),
                            name: "episode_commit".into(),
                            arguments: serde_json::json!({
                                "summary": "Chose SQLite single handle over Postgres",
                                "why_and_tradeoffs": ["Single SQLite handle avoids cross-process lock contention"],
                                "settled_decisions": ["Use SQLite single handle"],
                                "rejected_options": ["Postgres"],
                                "entities": ["SQLite", "Postgres"],
                                "primary_memory_update": "Project uses a single SQLite handle",
                                "primary_kind": "decision",
                                "core_facts": [{"content": "Postgres was rejected as overkill", "kind": "fact"}]
                            })
                            .to_string(),
                            extra_content: None,
                        },
                    ],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Staged and committed.".into()),
                    tool_calls: Vec::new(),
                    usage: None,
                    reasoning_content: None,
                },
            ],
        );

        let mut cfg = Config::default();
        cfg.memory.types.enabled = true;
        cfg.memory.consolidation_extract_facts = true;

        let outcome = SharedOutcome::new();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(SessionSearchTool {
                session_backend: Arc::clone(&sessions),
                allowed_session_keys: vec!["sess_1".into()],
            }),
            Box::new(EpisodeReadTool {
                memory: Arc::clone(&mem),
                agent_id: "nova".into(),
            }),
            Box::new(CheckpointStageTool {
                session_backend: Arc::clone(&sessions),
                session_key: "sess_1".into(),
                max_message_id: 2,
                outcome: outcome.clone(),
            }),
            Box::new(EpisodeCommitTool {
                memory: Arc::clone(&mem),
                memory_config: cfg.memory.clone(),
                agent_id: "nova".into(),
                default_session_key: "sess_1".into(),
                source_kind: EpisodeSourceKind::Dialogue,
                default_started_at: "2026-11-18T01:00:00Z".into(),
                default_ended_at: "2026-11-18T02:00:00Z".into(),
                default_up_to_message_id: Some(2),
                tolerate_unsupported_sleep: false,
                outcome: outcome.clone(),
            }),
            Box::new(MemorySupersedeTool {
                memory: Arc::clone(&mem),
                agent_id: "nova".into(),
                outcome: outcome.clone(),
            }),
        ];

        run_confined_loop(
            &provider,
            "custom",
            "gpt-5",
            None,
            "nova",
            tools,
            WATERMARK_COMPACTOR_SYSTEM_PROMPT,
            "Compact sess_1",
            4,
        )
        .await
        .unwrap();

        let exposed = provider.seen_tools.lock().clone();
        assert!(!exposed.is_empty());
        for name in &exposed[0] {
            assert!(
                CONFINED_SLEEP_TOOL_NAMES.contains(&name.as_str()),
                "unexpected non-sleep tool exposed: {name}"
            );
        }
        assert!(!exposed[0].iter().any(|n| matches!(
            n.as_str(),
            "shell" | "file_read" | "file_write" | "http_request"
        )));

        let window = sessions.load_session_window("sess_1").unwrap();
        assert_eq!(
            window.staged_checkpoint.as_ref().unwrap().up_to_message_id,
            2
        );

        let episodes = mem
            .read_episodes_in_window("nova", None, None, Some("sess_1"), 10)
            .await
            .unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(
            episodes[0].why_and_tradeoffs,
            vec!["Single SQLite handle avoids cross-process lock contention"]
        );
        assert_eq!(
            episodes[0].settled_decisions,
            vec!["Use SQLite single handle"]
        );
        assert_eq!(episodes[0].rejected_options, vec!["Postgres"]);

        let core_rows = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        assert!(core_rows.iter().any(|r| {
            r.content.contains("single SQLite handle")
                && r.kind == Some(MemoryKind::Semantic(SemanticSubtype::Decision))
        }));
        assert!(core_rows.iter().any(|r| {
            r.content.contains("Postgres was rejected")
                && r.kind == Some(MemoryKind::Semantic(SemanticSubtype::Fact))
        }));
    }

    #[tokio::test]
    async fn consolidate_turn_via_sleep_parity_honors_policy_dedup_conflict_and_provider_fail() {
        let (_dir, store, _sessions) = sleep_fixture();
        let mem: Arc<dyn Memory> = store.clone();
        let mut cfg = MemoryConfig::default();
        cfg.types.enabled = true;
        cfg.consolidation_extract_facts = true;
        cfg.conflict_supersede_enabled = true;

        let err = consolidate_turn_via_sleep(
            &FailingProvider,
            "m",
            None,
            Arc::clone(&mem),
            &cfg,
            "nova",
            "sess_p",
            "User prefers dark mode",
            "Got it",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("synthetic provider error"));
        assert!(mem.list(None, None).await.unwrap().is_empty());

        let provider = ScriptedNativeProvider::new(
            "parity",
            vec![ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "episode_commit".into(),
                    arguments: serde_json::json!({
                        "summary": "Owner switched editor theme to light mode",
                        "why_and_tradeoffs": ["High daylight glare"],
                        "settled_decisions": ["Use light mode"],
                        "rejected_options": ["Dark mode"],
                        "primary_memory_update": "Owner prefers light mode",
                        "primary_kind": "preference",
                        "core_facts": [{"content": "Daylight glare motivates light mode", "kind": "fact"}]
                    })
                    .to_string(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            }],
        );
        consolidate_turn_via_sleep(
            &provider,
            "m",
            None,
            Arc::clone(&mem),
            &cfg,
            "nova",
            "sess_p",
            "Switch to light mode because of glare",
            "Switched to light mode.",
        )
        .await
        .unwrap();

        let episodes = mem
            .read_episodes_in_window("nova", None, None, Some("sess_p"), 5)
            .await
            .unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].why_and_tradeoffs, vec!["High daylight glare"]);
        let cores = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        assert!(cores.iter().any(|c| {
            c.content == "Owner prefers light mode"
                && c.kind == Some(MemoryKind::Semantic(SemanticSubtype::Preference))
        }));
    }

    #[tokio::test]
    async fn watermark_compaction_stages_at_65_percent_and_promotes_at_90_percent() {
        let (_dir, store, sessions) = sleep_fixture();
        let mem: Arc<dyn Memory> = store.clone();
        sessions
            .append("ws_sess", &ChatMessage::user("First message"))
            .unwrap();
        sessions
            .append("ws_sess", &ChatMessage::assistant("First reply"))
            .unwrap();

        let mut config = Config::default();
        config.providers.models.openai.insert(
            "primary".into(),
            OpenAIModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("gpt-5".into()),
                    uri: Some("http://127.0.0.1:9".into()),
                    api_key: Some("test-key".into()),
                    ..Default::default()
                },
            },
        );

        // Below 65% watermark (600 / 1000) -> does nothing even with unreachable URI.
        maybe_compact_ws_turn(
            &config,
            Some(&sessions),
            Some(&mem),
            "nova",
            "ws_sess",
            "openai.primary",
            "gpt-5",
            1000,
            Some(600),
        )
        .await
        .unwrap();
        let w = sessions.load_session_window("ws_sess").unwrap();
        assert!(w.staged_checkpoint.is_none());
        assert!(w.active_checkpoint.is_none());

        // Cold-window promotion test: stage a checkpoint and backdate its check via direct promotion at >= 90%.
        sessions
            .stage_checkpoint("ws_sess", 2, "Rolling staged digest at 65%")
            .unwrap();
        // At 70% with current watermark already staged -> keeps staged, does not promote yet.
        maybe_compact_ws_turn(
            &config,
            Some(&sessions),
            Some(&mem),
            "nova",
            "ws_sess",
            "openai.primary",
            "gpt-5",
            1000,
            Some(700),
        )
        .await
        .unwrap();
        let w70 = sessions.load_session_window("ws_sess").unwrap();
        assert!(w70.staged_checkpoint.is_some());
        assert!(w70.active_checkpoint.is_none());

        // At 90% -> promotes staged checkpoint to active!
        maybe_compact_ws_turn(
            &config,
            Some(&sessions),
            Some(&mem),
            "nova",
            "ws_sess",
            "openai.primary",
            "gpt-5",
            1000,
            Some(900),
        )
        .await
        .unwrap();
        let w90 = sessions.load_session_window("ws_sess").unwrap();
        assert!(w90.staged_checkpoint.is_none());
        assert_eq!(w90.active_checkpoint.unwrap().up_to_message_id, 2);
    }

    #[tokio::test]
    async fn startup_and_idle_flush_commits_episode_and_promotes_staged_checkpoint() {
        let (_dir, store, sessions) = sleep_fixture();
        let mem: Arc<dyn Memory> = store.clone();
        sessions
            .append("idle_sess", &ChatMessage::user("Remember we chose Rust."))
            .unwrap();
        sessions
            .append("idle_sess", &ChatMessage::assistant("Noted: Rust chosen."))
            .unwrap();
        sessions
            .stage_checkpoint("idle_sess", 2, "Staged before idle")
            .unwrap();

        let cfg = Config::default();
        let flushed = flush_session_to_episode(
            &cfg,
            &sessions,
            &mem,
            "nova",
            "idle_sess",
            EpisodeSourceKind::StartupRecovery,
            None,
        )
        .await
        .unwrap();
        assert!(flushed);

        let episodes = mem
            .read_episodes_in_window("nova", None, None, Some("idle_sess"), 5)
            .await
            .unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].source_kind, EpisodeSourceKind::StartupRecovery);
        assert_eq!(episodes[0].up_to_message_id, Some(2));

        let window = sessions.load_session_window("idle_sess").unwrap();
        assert!(window.staged_checkpoint.is_none());
        assert_eq!(window.active_checkpoint.unwrap().up_to_message_id, 2);

        // Re-running flush when no new delta arrived is a no-op.
        let flushed_again = flush_session_to_episode(
            &cfg,
            &sessions,
            &mem,
            "nova",
            "idle_sess",
            EpisodeSourceKind::IdleFlush,
            None,
        )
        .await
        .unwrap();
        assert!(!flushed_again);
    }

    #[tokio::test]
    async fn deep_dream_preserves_owner_edited_daily_digest_and_keeps_action_candidates_pending() {
        let (_dir, store, sessions) = sleep_fixture();
        let mem: Arc<dyn Memory> = store.clone();

        mem.store_with_agent(
            "pref_theme",
            "Dark theme",
            MemoryCategory::Core,
            None,
            Some("default"),
            Some(0.7),
            Some("nova"),
        )
        .await
        .unwrap();
        mem.commit_episode(&EpisodeRecord {
            episode_id: "ep_dream_1".into(),
            agent_id: "nova".into(),
            session_key: "sess_d".into(),
            started_at: "2026-11-17T10:00:00Z".into(),
            ended_at: "2026-11-17T11:00:00Z".into(),
            up_to_message_id: Some(4),
            summary: "Discussed switching to solarized theme and scheduling weekly backup".into(),
            why_and_tradeoffs: vec!["Better contrast outdoors".into()],
            settled_decisions: vec!["Use solarized theme".into()],
            rejected_options: vec!["Pure white".into()],
            entities: vec!["zeroclaw".into()],
            source_kind: EpisodeSourceKind::Dialogue,
        })
        .await
        .unwrap();

        let window = DreamWindow {
            day_key: "2026-11-18".into(),
            window_start: "2026-11-17T04:00:00Z".into(),
            window_end: "2026-11-18T04:00:00Z".into(),
        };

        let provider = ScriptedNativeProvider::new(
            "dream",
            vec![ChatResponse {
                text: None,
                tool_calls: vec![
                    ToolCall {
                        id: "s1".into(),
                        name: "memory_supersede".into(),
                        arguments: serde_json::json!({
                            "old_key": "pref_theme",
                            "new_key": "pref_theme_v2",
                            "new_content": "Solarized theme"
                        })
                        .to_string(),
                        extra_content: None,
                    },
                    ToolCall {
                        id: "a1".into(),
                        name: "action_propose".into(),
                        arguments: serde_json::json!({
                            "action": "Schedule weekly backup cron",
                            "rationale": "Owner mentioned wanting automatic weekly backups"
                        })
                        .to_string(),
                        extra_content: None,
                    },
                    ToolCall {
                        id: "d1".into(),
                        name: "digest_publish".into(),
                        arguments: serde_json::json!({
                            "narrative": "Machine synthesis for 2026-11-18",
                            "settled_decisions": ["Use solarized theme"],
                            "why_and_tradeoffs": ["Better contrast outdoors"],
                            "latent_insights": ["Backup cadence aligns with Sunday maintenance"],
                            "entities": ["zeroclaw"],
                            "pinned_index": "Active: solarized theme; entities: zeroclaw"
                        })
                        .to_string(),
                        extra_content: None,
                    },
                ],
                usage: None,
                reasoning_content: None,
            }],
        );

        let mut cfg = Config::default();
        cfg.gateway.attention = Some(AttentionConfig {
            timezone: "UTC".into(),
            quiet_start: "22:00".into(),
            quiet_end: "08:00".into(),
            important_sources: Vec::new(),
        });

        let dream_model = ResolvedSleepModel {
            provider: Box::new(provider),
            provider_name: "custom".into(),
            model: "flagship".into(),
            temperature: None,
        };

        let ran = run_deep_dream_for_agent(
            &cfg,
            &sessions,
            &mem,
            "nova",
            &window,
            &dream_model,
            "2026-11-18T04:05:00Z".parse().unwrap(),
        )
        .await
        .unwrap();
        assert!(ran);

        let digests = mem.load_daily_digests("nova", 5).await.unwrap();
        assert_eq!(digests.len(), 1);
        assert!(!digests[0].owner_edited);
        assert_eq!(digests[0].action_candidates.len(), 1);
        let cand_key = &digests[0].action_candidates[0];
        assert_eq!(
            digests[0].action_decisions.get(cand_key).copied(),
            Some(ActionCandidateStatus::Pending)
        );
        let pinned = mem.load_pinned_digest("nova").await.unwrap().unwrap();
        assert!(pinned.rendered_index.len() <= PINNED_DIGEST_MAX_BYTES);

        let mut edited = digests[0].clone();
        edited.narrative = "Owner curated narrative — do not clobber".into();
        edited.owner_edited = true;
        mem.publish_daily_digest(&edited).await.unwrap();

        let provider2 = ScriptedNativeProvider::new(
            "dream2",
            vec![ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "d2".into(),
                    name: "digest_publish".into(),
                    arguments: serde_json::json!({
                        "narrative": "Machine overwrite attempt",
                        "pinned_index": "Refreshed pinned digest from owner edit"
                    })
                    .to_string(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            }],
        );
        let dream_model2 = ResolvedSleepModel {
            provider: Box::new(provider2),
            provider_name: "custom".into(),
            model: "flagship".into(),
            temperature: None,
        };
        run_deep_dream_for_agent(
            &cfg,
            &sessions,
            &mem,
            "nova",
            &window,
            &dream_model2,
            "2026-11-18T05:00:00Z".parse().unwrap(),
        )
        .await
        .unwrap();

        let after = mem.load_daily_digests("nova", 5).await.unwrap();
        assert_eq!(
            after[0].narrative,
            "Owner curated narrative — do not clobber"
        );
        assert!(after[0].owner_edited);
        let refreshed_pinned = mem.load_pinned_digest("nova").await.unwrap().unwrap();
        assert_eq!(
            refreshed_pinned.rendered_index,
            "Refreshed pinned digest from owner edit"
        );
    }

    #[test]
    fn dream_window_and_quiet_hours_handle_0400_cut_and_catchup() {
        let tz = jiff::tz::TimeZone::UTC;
        let before_cut =
            due_dream_window("2026-11-18T03:59:00Z".parse().unwrap(), tz.clone()).unwrap();
        assert_eq!(before_cut.day_key, "2026-11-17");

        let at_cut = due_dream_window("2026-11-18T04:00:00Z".parse().unwrap(), tz.clone()).unwrap();
        assert_eq!(at_cut.day_key, "2026-11-18");
        assert_eq!(
            zeroclaw_api::time::parse_rfc3339(&at_cut.window_start).unwrap(),
            "2026-11-17T04:00:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert_eq!(
            zeroclaw_api::time::parse_rfc3339(&at_cut.window_end).unwrap(),
            "2026-11-18T04:00:00Z".parse::<jiff::Timestamp>().unwrap()
        );

        let wake = due_dream_window("2026-11-18T10:15:00Z".parse().unwrap(), tz).unwrap();
        assert_eq!(wake.day_key, "2026-11-18");

        let policy = AttentionConfig {
            timezone: "UTC".into(),
            quiet_start: "22:00".into(),
            quiet_end: "08:00".into(),
            important_sources: Vec::new(),
        };
        assert!(is_quiet_hours(
            Some(&policy),
            "2026-11-18T04:05:00Z".parse().unwrap()
        ));
        assert!(!is_quiet_hours(
            Some(&policy),
            "2026-11-18T10:05:00Z".parse().unwrap()
        ));
    }

    #[tokio::test]
    async fn soul_namespace_isolation_refuses_soul_targets() {
        let (_dir, store, _sessions) = sleep_fixture();
        let mem: Arc<dyn Memory> = store.clone();
        let tool = MemorySupersedeTool {
            memory: Arc::clone(&mem),
            agent_id: "nova".into(),
            outcome: SharedOutcome::new(),
        };
        let err = tool
            .execute(serde_json::json!({
                "old_key": "soul::story",
                "new_key": "other",
                "new_content": "hijack"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Soul"));
    }

    #[cfg(feature = "memory-tachi")]
    #[tokio::test]
    async fn tachi_end_to_end_sleep_compactor_parity() {
        let dir = TempDir::new().unwrap();
        let tachi = Arc::new(zeroclaw_memory::TachiMemory::new("tachi", dir.path()).unwrap());
        let mem: Arc<dyn Memory> = tachi;
        let mut cfg = MemoryConfig::default();
        cfg.types.enabled = true;
        cfg.consolidation_extract_facts = true;

        let provider = ScriptedNativeProvider::new(
            "tachi-parity",
            vec![ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "episode_commit".into(),
                    arguments: serde_json::json!({
                        "summary": "Adopted ADR-025 sleep compactor on TachiMemory",
                        "why_and_tradeoffs": ["Single SQLite store avoids handle drift"],
                        "settled_decisions": ["Use TachiMemory sleep traits"],
                        "rejected_options": ["Dual SQLite stores"],
                        "primary_memory_update": "TachiMemory hosts sleep records",
                        "primary_kind": "decision",
                        "core_facts": [{"content": "Dual SQLite stores were rejected", "kind": "fact"}]
                    })
                    .to_string(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            }],
        );
        consolidate_turn_via_sleep(
            &provider,
            "m",
            None,
            Arc::clone(&mem),
            &cfg,
            "nova",
            "sess_tachi",
            "Let's keep sleep records in TachiMemory",
            "Agreed, single TachiMemory handle.",
        )
        .await
        .unwrap();

        let eps = mem
            .read_episodes_in_window("nova", None, None, Some("sess_tachi"), 5)
            .await
            .unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(
            eps[0].settled_decisions,
            vec!["Use TachiMemory sleep traits"]
        );
        let cores = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        assert!(
            cores
                .iter()
                .any(|c| c.content == "TachiMemory hosts sleep records")
        );
        assert!(
            cores
                .iter()
                .any(|c| c.content == "Dual SQLite stores were rejected")
        );
    }
}
