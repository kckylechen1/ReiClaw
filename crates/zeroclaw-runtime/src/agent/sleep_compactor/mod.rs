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

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use zeroclaw_api::memory_traits::{
    DailyDigest, EpisodeRecord, EpisodeSourceKind, Memory, MemoryCategory, MemoryEntry, MemoryKind,
    SemanticSubtype, StoreOptions, validate_non_soul_key,
};
use zeroclaw_api::tool::Tool;
use zeroclaw_config::schema::{Config, MemoryConfig, MultimodalConfig, PacingConfig};
use zeroclaw_infra::session_backend::{SessionBackend, SessionCheckpoint};
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
pub(super) const DREAM_STATE_FILENAME: &str = "sleep_dream_state.json";

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

pub(super) const EPISODE_FLUSH_SYSTEM_PROMPT: &str = r#"You are a confined sleep-time episode compiler.
Distill the provided unflushed session transcript into a durable `EpisodeRecord` via `episode_commit` (and `memory_supersede` if a prior Core fact was explicitly superseded).

Preserve:
- User and assistant Thoughts (`why_and_tradeoffs`)
- `settled_decisions`
- `rejected_options`
- `entities`
- Durable `core_facts` (if any)

Never propose or execute external actions here unless clearly warranted; keep summaries grounded strictly in the transcript."#;

pub(super) const DEEP_DREAM_SYSTEM_PROMPT: &str = r#"You are the overnight 04:00 Deep Auto Dream synthesizer for a single personal agent.
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

pub(super) fn truncate_utf8(text: &str, max_bytes: usize) -> String {
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
pub(super) struct ExtractedCoreFact {
    pub(super) content: String,
    #[serde(default)]
    pub(super) kind: Option<String>,
    #[serde(default)]
    pub(super) key: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct SleepPassOutcome {
    pub(super) staged_checkpoint: Option<SessionCheckpoint>,
    pub(super) committed_episodes: Vec<String>,
    pub(super) superseded_facts: Vec<String>,
    pub(super) proposed_actions: Vec<String>,
    pub(super) published_digest: Option<DailyDigest>,
    pub(super) preserved_owner_digest: bool,
}

#[derive(Clone)]
pub(super) struct SharedOutcome(pub(super) Arc<Mutex<SleepPassOutcome>>);

impl SharedOutcome {
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(SleepPassOutcome::default())))
    }
}

pub(super) fn string_array_arg(args: &serde_json::Value, key: &str) -> Vec<String> {
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

pub(super) fn is_unsupported_sleep_error(err: &anyhow::Error) -> bool {
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

pub(super) async fn persist_extracted_core_facts(
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

mod dream;
mod tools;

pub use dream::{
    DreamWindow, due_dream_window, flush_session_to_episode, is_quiet_hours,
    run_deep_dream_for_agent, run_worker,
};
use tools::{
    CheckpointStageTool, EpisodeCommitTool, EpisodeReadTool, MemorySupersedeTool, SessionSearchTool,
};
pub(super) async fn run_confined_loop(
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
    let mut window =
        tokio::task::spawn_blocking(move || backend_clone.load_session_window(&key_owned))
            .await
            .context("load_session_window task failed")??;

    // 1. Cold-window promotion: if a staged checkpoint has sat through a cold gap,
    //    promote it immediately and reload `window` so delta/watermark checks see
    //    the post-promotion snapshot.
    let now_ts = jiff::Timestamp::now();
    if let Some(ref staged) = window.staged_checkpoint {
        let age_secs = now_ts
            .as_second()
            .saturating_sub(staged.updated_at.as_second());
        if age_secs >= COLD_WINDOW_GAP_SECS {
            let b = Arc::clone(backend);
            let k = session_key.to_string();
            if let Ok(Ok(Some(_))) =
                tokio::task::spawn_blocking(move || b.promote_staged_checkpoint(&k)).await
            {
                let b_reload = Arc::clone(backend);
                let k_reload = session_key.to_string();
                window =
                    tokio::task::spawn_blocking(move || b_reload.load_session_window(&k_reload))
                        .await
                        .context("load_session_window reload task failed")??;
            }
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

pub(super) fn format_delta_transcript(
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

pub(super) fn build_fallback_digest(final_text: &str, transcript: &str) -> String {
    let trimmed = final_text.trim();
    if !trimmed.is_empty() {
        truncate_utf8(trimmed, 4_000)
    } else {
        truncate_utf8(transcript, 1_500)
    }
}

#[cfg(test)]
mod tests;
