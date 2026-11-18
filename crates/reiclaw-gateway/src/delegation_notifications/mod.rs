//! #63 delegated-result delivery: observe terminal states of bound Tachi
//! dispatches and enqueue one attention notice per canonical outcome into the
//! existing bridge outbox. No second task table, notifier, policy, or store —
//! the outbox's `(source_kind, source_id, event_id)` identity de-duplicates,
//! the attention policy (#447) gates delivery, and execution state remains
//! owned by Tachi's receipts. A status query failure is an unknown outcome
//! and is never itself a notification.
//!
//! Identity rules this producer owes (review round 1 of #566):
//! - `source_id` is `agent \x1f request_id` — the outbox row carries BOTH the
//!   owning agent (for delivery gating and per-source mutes) and the request,
//!   so two agents sharing a request_id never collapse into one notification.
//! - Route provenance: a dispatch is only queried when the CURRENT `[tachi]`
//!   route fingerprint equals the binding's stored digest — the same
//!   `route_conflict` protection the interactive tools apply.
//! - The binding's `notified_outcome` marker outlives the outbox's dedup
//!   retention, so an evicted receipt can never resurrect a notification.
//! - A destination is `(bridge, recipient, thread_id)` in BOTH the notified
//!   marker and the outbox `event_id`, matching what `permits_delivery`
//!   gates on, so retargeting a queued notice to another thread of the same
//!   recipient enqueues a fresh notice instead of being suppressed (#566
//!   review).
//! - Pagination rotates a rowid cursor across passes: bounded pages, no
//!   starvation of later rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::RwLock;
use reiclaw_api::model_provider::ModelProvider;
use reiclaw_api::sleep_records::{EpisodeRecord, EpisodeSourceKind};
use reiclaw_api::tachi_staff::{ContinueReceipt, RunResult, RunState, RunStatus, TachiStaffError};
use reiclaw_config::schema::Config;
use reiclaw_infra::bridge_outbox::BridgeOutbox;
use reiclaw_infra::session_backend::SessionBackend;
use reiclaw_infra::session_sqlite::SqliteSessionBackend;
use reiclaw_memory::Memory;
use reiclaw_providers::ProviderDispatch;
use reiclaw_tools::tachi_delegation::{
    MAX_PROXY_CONTINUATIONS_PER_DISPATCH, ProxyContinuationError, ProxyContinuationGate,
    find_protected_identity_marker,
};

/// Status fetcher: (agent, dispatch, validated route snapshot) → status.
pub(crate) type StatusFetch<'a> = &'a (
        dyn Fn(
    &str,
    &str,
    &reiclaw_config::tachi::TachiConfig,
) -> BoxFuture<'static, anyhow::Result<RunStatus>>
            + Send
            + Sync
    );

/// Result fetcher: (agent, dispatch, validated route snapshot) → canonical report.
pub(crate) type ResultFetch<'a> = &'a (
        dyn Fn(
    &str,
    &str,
    &reiclaw_config::tachi::TachiConfig,
) -> BoxFuture<'static, anyhow::Result<RunResult>>
            + Send
            + Sync
    );

/// Continuation sender: (agent, dispatch, validated route snapshot, expected_revision, answer) → ContinueReceipt.
pub(crate) type ContinueSender<'a> = &'a (
        dyn Fn(
    &str,
    &str,
    &reiclaw_config::tachi::TachiConfig,
    u64,
    &str,
) -> BoxFuture<'static, Result<ContinueReceipt, TachiStaffError>>
            + Send
            + Sync
    );

const DELEGATION: &str = "delegation";
const INPUT_REQUIRED_OUTCOME: &str = "input_required";
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const PAGE_SIZE: usize = 500;
const MAX_EPISODE_DIGEST_BYTES: usize = 4_096;
const MAX_PROMPT_EXTRACT_BYTES: usize = 2_048;
const MAX_REASON_EXCERPT_BYTES: usize = 160;
const INSUFFICIENT_FACTS_SENTINEL: &str = "INSUFFICIENT_FACTS";
const PROXY_SYSTEM_PROMPT: &str = "\
You are the owner's personal controller answering an external delegated worker's \
INPUT_REQUIRED question using ONLY the provided grounded session and memory facts.\n\
Rules:\n\
- Answer concisely and factually using ONLY the provided grounded facts.\n\
- If the facts do not clearly and unambiguously answer the question, or if the \
question asks for authorization, approval, credentials, personal/persona preferences, \
or subjective owner decisions, reply with the exact token INSUFFICIENT_FACTS.\n\
- Never include credentials, secrets, filesystem paths, shell commands, or Soul/User Model content.";

/// Destination configuration owns the target; the startup data root owns this
/// gateway's stores (a changed root waits for the owning gateway to restart).
fn eligible(config: &Config, data_dir: &Path) -> bool {
    config.data_dir == data_dir
        && config.tachi.enabled
        && config
            .tachi
            .result_notification
            .as_ref()
            .is_some_and(|target| {
                target.validate().is_ok() && config.gateway.bridges.contains_key(&target.bridge)
            })
}

/// The outcome a terminal projection maps to, or `None` when the run is not
/// (yet) attention-worthy. `InputRequired` counts only when the daemon closed
/// it as `partial` — the same terminal rule the status projections use.
fn terminal_outcome(status: &RunStatus) -> Option<&'static str> {
    match status.state {
        RunState::Completed => Some("completed"),
        RunState::Failed => Some("failed"),
        RunState::Canceled => Some("canceled"),
        RunState::InputRequired if status.closure_kind.as_deref() == Some("partial") => {
            Some("partial")
        }
        _ => None,
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// The outbox source identity: owning agent + request. Admitted request ids
/// exclude the separator, so the final separator unambiguously splits them.
fn source_identity(agent: &str, request_id: &str) -> String {
    format!("{agent}\u{1f}{request_id}")
}

/// The owning agent encoded in a delegation outbox row's source identity.
fn owning_agent(source_id: &str) -> &str {
    source_id
        .rsplit_once('\u{1f}')
        .map_or("", |(agent, _)| agent)
}

/// Scan position across passes: `cursor` is the last rowid examined; a wrap
/// happens ONLY on a short page (natural end of the rowid order). Monotone
/// consumption cannot starve any row: new claims always land at rowids ABOVE
/// every existing one, so unvisited older rows are always consumed before
/// newer inserts, and the sweep reaches its natural end in
/// ceil(total/page_size) passes. (A restart resets the cursor — coverage is
/// delayed, never lost.) Tests shrink `page_size` to exercise pagination.
pub(crate) struct Rotation {
    pub cursor: i64,
    pub page_size: usize,
    /// Full pages seen since the last wrap.
    pub full_pages_since_wrap: usize,
    /// The table's highest rowid at sweep start (0 = new sweep). A forced
    /// rewind fires only when a page contains rows BEYOND this extent —
    /// churn is outrunning the sweep — so a finite static table always
    /// walks to its natural end at ANY depth, while sustained insertion
    /// bounds head re-observation: the wait is finite and grows with sweep
    /// depth (the rewind fires once the sweep passes the recorded extent
    /// with the counter at threshold), not a fixed every-N-pages cadence.
    pub sweep_max_rowid: i64,
}

const REWIND_PASSES: usize = 8;

impl Rotation {
    fn wrap(&mut self) {
        self.cursor = 0;
        self.full_pages_since_wrap = 0;
        self.sweep_max_rowid = 0;
    }
}

impl Default for Rotation {
    fn default() -> Self {
        Self {
            cursor: 0,
            page_size: PAGE_SIZE,
            full_pages_since_wrap: 0,
            sweep_max_rowid: 0,
        }
    }
}

/// The persistent notified marker: a bounded, `\x1e`-separated list of
/// destination-scoped entries (most recent last). Every destination this
/// outcome was already notified to is remembered, so a target flip A→B→A
/// does not re-notify A after outbox retention evicted A's receipt — while a
/// genuinely new destination still gets its notice. The list is capped
/// (oldest entries dropped) so a lifetime of target churn cannot grow it
/// unboundedly.
///
/// A destination is `(bridge, recipient, thread_id)` — the SAME triple
/// [`permits_delivery`] gates on. Entries written before the thread became
/// part of the identity carry only `outcome \x1f bridge \x1f recipient`;
/// an existing legacy outbox receipt recovers its exact thread. Without
/// that evidence, retain conservative suppression for that recipient rather
/// than risk re-sending an expired or uncertain notification.
const MARKER_LIST_CAP: usize = 8;

struct MarkerUpdate {
    rowid: i64,
    entry: String,
    replaces: Option<String>,
}

/// Encode an optional thread so `None` and every `Some(_)` are distinct and
/// no thread value can collide with another (the `=` prefix keeps an empty
/// thread distinct from "no thread").
fn thread_token(thread_id: Option<&str>) -> String {
    match thread_id {
        None => "-".to_string(),
        Some(thread) => format!("={thread}"),
    }
}

fn marker_entry(outcome: &str, bridge: &str, recipient: &str, thread_id: Option<&str>) -> String {
    format!(
        "{outcome}\u{1f}{bridge}\u{1f}{recipient}\u{1f}{}",
        thread_token(thread_id)
    )
}

/// The pre-thread (legacy) form of an entry, matched for compatibility.
fn legacy_marker_entry(outcome: &str, bridge: &str, recipient: &str) -> String {
    format!("{outcome}\u{1f}{bridge}\u{1f}{recipient}")
}

/// The outbox `event_id` for one outcome at one destination. The outbox
/// dedupes on `(bridge, recipient, source_kind, source_id, event_id)`, which
/// has no thread column, so the thread is folded into the event id: a notice
/// queued for thread A and a retarget to thread B of the SAME recipient are
/// two rows, each gated by [`permits_delivery`] on its own thread. The
/// thread-less destination is also explicit: a legacy threaded row used the
/// bare outcome, and must not absorb a new notice targeting no thread.
fn destination_event_id(outcome: &str, thread_id: Option<&str>) -> String {
    format!("{outcome}\u{1f}thread\u{1f}{}", thread_token(thread_id))
}

/// True when this outcome was already enqueued to this exact destination.
/// Legacy markers need the receipt lookup in `reconcile` before they can be
/// interpreted as an exact destination.
fn already_notified(
    list: &str,
    outcome: &str,
    bridge: &str,
    recipient: &str,
    thread_id: Option<&str>,
) -> bool {
    marker_contains(list, &marker_entry(outcome, bridge, recipient, thread_id))
}

/// True when the marker list already contains `entry`.
fn marker_contains(list: &str, entry: &str) -> bool {
    list.split('\u{1e}').any(|item| item == entry)
}

/// Append `entry` to the list, dropping the oldest beyond the cap.
fn marker_push(list: &str, entry: &str) -> String {
    let mut items: Vec<&str> = list
        .split('\u{1e}')
        .filter(|item| !item.is_empty() && *item != entry)
        .collect();
    items.push(entry);
    let start = items.len().saturating_sub(MARKER_LIST_CAP);
    items[start..].join("\u{1e}")
}

/// Context passed to a [`ContinuationResolver`] when a delegated run enters
/// non-partial `TASK_STATE_INPUT_REQUIRED`.
#[allow(dead_code)]
pub(crate) struct ContinuationContext<'a> {
    pub agent_alias: &'a str,
    pub request_id: &'a str,
    pub dispatch_id: &'a str,
    pub revision: u64,
    pub prompt: Option<&'a str>,
    pub read_projection: Option<&'a serde_json::Value>,
}

/// Grounded proxy-continuation responder for live `INPUT_REQUIRED` pauses.
/// Implementations must return `Ok(None)` whenever facts are absent or
/// insufficient so the owner receives an `input_required` attention notice.
#[async_trait::async_trait]
pub(crate) trait ContinuationResolver: Send + Sync {
    async fn resolve(&self, ctx: &ContinuationContext<'_>) -> anyhow::Result<Option<String>>;
}

/// [`ContinuationResolver`] backed by `ProviderDispatch` and grounded technical
/// facts from `sessions.db` and non-Soul `Memory` records.
///
/// Explicitly excludes `soul.db`, `user_model.db`, and any protected Soul/User
/// Model markers; strictly returns `Ok(None)` when prompt text or grounded facts
/// are insufficient.
pub(crate) struct GroundedContinuationResolver {
    provider: Arc<dyn ModelProvider>,
    model: String,
    temperature: Option<f64>,
    data_dir: PathBuf,
    memory: Option<Arc<dyn Memory>>,
}

impl GroundedContinuationResolver {
    #[allow(dead_code)]
    pub(crate) fn new(
        provider: Arc<dyn ModelProvider>,
        model: impl Into<String>,
        temperature: Option<f64>,
        data_dir: PathBuf,
        memory: Option<Arc<dyn Memory>>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            temperature,
            data_dir,
            memory,
        }
    }

    fn is_excluded_memory_scope(namespace: &str, key: &str, content: &str) -> bool {
        let ns_lower = namespace.trim().to_ascii_lowercase();
        if ns_lower == "soul"
            || ns_lower.starts_with("soul::")
            || ns_lower.contains("user_model")
            || ns_lower.contains("soul.db")
            || ns_lower.contains("user_model.db")
        {
            return true;
        }
        find_protected_identity_marker(key).is_some()
            || find_protected_identity_marker(content).is_some()
    }

    fn collect_session_facts(&self, agent_alias: &str, prompt: &str) -> Vec<String> {
        let Ok(sessions) = SqliteSessionBackend::new(&self.data_dir) else {
            return Vec::new();
        };
        let mut metas: Vec<_> = sessions
            .list_sessions_with_metadata()
            .into_iter()
            .filter(|meta| meta.agent_alias.as_deref() == Some(agent_alias))
            .collect();
        metas.sort_by_key(|b| std::cmp::Reverse(b.last_activity));

        let mut facts = Vec::new();
        let allowed_keys: Vec<String> = metas.iter().take(16).map(|m| m.key.clone()).collect();
        for key in allowed_keys.iter().take(3) {
            if let Ok(window) = sessions.load_session_window(key) {
                for cp in [window.active_checkpoint, window.staged_checkpoint]
                    .into_iter()
                    .flatten()
                {
                    let scrubbed = reiclaw_providers::scrub_secret_patterns(&cp.digest);
                    let trimmed = scrubbed.trim();
                    if !trimmed.is_empty() && find_protected_identity_marker(trimmed).is_none() {
                        facts.push(format!(
                            "Session checkpoint ({}): {}",
                            key,
                            truncate_utf8(trimmed, 800)
                        ));
                    }
                }
            }
        }
        if !allowed_keys.is_empty() {
            let key_refs: Vec<&str> = allowed_keys.iter().map(String::as_str).collect();
            if let Ok(hits) = sessions.search_session_fts(&key_refs, prompt, 5) {
                for hit in hits {
                    let scrubbed = reiclaw_providers::scrub_secret_patterns(&hit.content);
                    let trimmed = scrubbed.trim();
                    if !trimmed.is_empty() && find_protected_identity_marker(trimmed).is_none() {
                        facts.push(format!(
                            "Session transcript ({}): {}",
                            hit.session_key,
                            truncate_utf8(trimmed, 500)
                        ));
                    }
                }
            }
        }
        facts
    }

    async fn collect_grounded_facts(&self, agent_alias: &str, prompt: &str) -> Vec<String> {
        let mut facts = self.collect_session_facts(agent_alias, prompt);
        let Some(mem) = self.memory.as_deref() else {
            return facts;
        };
        if mem.name() == "none" {
            return facts;
        }
        if let Ok(episodes) = mem
            .read_episodes_in_window(agent_alias, None, None, None, 5)
            .await
        {
            for ep in episodes {
                let scrubbed = reiclaw_providers::scrub_secret_patterns(&ep.summary);
                let trimmed = scrubbed.trim();
                if !trimmed.is_empty() && find_protected_identity_marker(trimmed).is_none() {
                    let mut line = format!(
                        "Episode ({}): {}",
                        ep.episode_id,
                        truncate_utf8(trimmed, 600)
                    );
                    if !ep.settled_decisions.is_empty() {
                        let decisions = ep
                            .settled_decisions
                            .iter()
                            .map(|d| reiclaw_providers::scrub_secret_patterns(d))
                            .filter(|d| find_protected_identity_marker(d).is_none())
                            .collect::<Vec<_>>()
                            .join("; ");
                        if !decisions.is_empty() {
                            line.push_str(&format!(" | Settled decisions: {decisions}"));
                        }
                    }
                    facts.push(line);
                }
            }
        }
        if let Ok(entries) = mem
            .recall_for_agents(&[agent_alias], prompt, 5, None, None, None)
            .await
        {
            for entry in entries {
                if Self::is_excluded_memory_scope(&entry.namespace, &entry.key, &entry.content) {
                    continue;
                }
                let scrubbed = reiclaw_providers::scrub_secret_patterns(&entry.content);
                let trimmed = scrubbed.trim();
                if !trimmed.is_empty() && find_protected_identity_marker(trimmed).is_none() {
                    facts.push(format!(
                        "Memory ({}): {}",
                        entry.key,
                        truncate_utf8(trimmed, 500)
                    ));
                }
            }
        }
        facts
    }
}

#[async_trait::async_trait]
impl ContinuationResolver for GroundedContinuationResolver {
    async fn resolve(&self, ctx: &ContinuationContext<'_>) -> anyhow::Result<Option<String>> {
        let Some(prompt) = ctx.prompt.map(str::trim).filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let facts = self.collect_grounded_facts(ctx.agent_alias, prompt).await;
        if facts.is_empty() {
            return Ok(None);
        }
        let user_prompt = format!(
            "Delegated request: {}\nWorker question:\n{}\n\nGrounded technical facts:\n- {}",
            ctx.request_id,
            prompt,
            facts.join("\n- ")
        );
        let raw_reply = ProviderDispatch::from_ref(&*self.provider)
            .chat_with_system(
                Some(PROXY_SYSTEM_PROMPT),
                &user_prompt,
                &self.model,
                self.temperature,
            )
            .await?;
        let trimmed = raw_reply.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case(INSUFFICIENT_FACTS_SENTINEL)
            || trimmed
                .to_ascii_uppercase()
                .starts_with(INSUFFICIENT_FACTS_SENTINEL)
        {
            return Ok(None);
        }
        let scrubbed = reiclaw_providers::scrub_secret_patterns(trimmed);
        let scrubbed = scrubbed.trim();
        if scrubbed.is_empty()
            || find_protected_identity_marker(scrubbed).is_some()
            || reiclaw_tools::tachi_admission::scan_text(scrubbed).is_err()
        {
            return Ok(None);
        }
        Ok(Some(scrubbed.to_string()))
    }
}

/// Dependencies injected into [`reconcile_with_deps`].
pub(crate) struct SweepDeps<'a> {
    pub fetch_status: StatusFetch<'a>,
    pub fetch_result: Option<ResultFetch<'a>>,
    pub send_continue: Option<ContinueSender<'a>>,
    pub continuation_resolver: Option<&'a dyn ContinuationResolver>,
    pub memory: Option<&'a dyn Memory>,
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn extract_prompt_from_value(value: &serde_json::Value, depth: u8) -> Option<String> {
    if depth == 0 {
        return None;
    }
    let obj = value.as_object()?;
    for key in [
        "question",
        "prompt",
        "input_prompt",
        "message",
        "detail",
        "reason",
        "summary",
    ] {
        if let Some(raw) = obj.get(key).and_then(serde_json::Value::as_str) {
            let scrubbed = reiclaw_providers::scrub_secret_patterns(raw);
            let trimmed = scrubbed.trim();
            if !trimmed.is_empty() && find_protected_identity_marker(trimmed).is_none() {
                return Some(truncate_utf8(trimmed, MAX_PROMPT_EXTRACT_BYTES).to_string());
            }
        }
    }
    for container in ["input_required", "pending_input", "question", "control"] {
        if let Some(nested) = obj.get(container)
            && let Some(found) = extract_prompt_from_value(nested, depth - 1)
        {
            return Some(found);
        }
    }
    None
}

/// Safely extract prompt/question context from `status.read_projection`.
/// Returns `None` conservatively when absent, empty, or containing protected markers.
pub(crate) fn extract_input_required_prompt(status: &RunStatus) -> Option<String> {
    status
        .read_projection
        .as_ref()
        .and_then(|projection| extract_prompt_from_value(projection, 3))
}

fn digest_result_body(body: &str) -> Option<String> {
    let scrubbed = reiclaw_providers::scrub_secret_patterns(body);
    let trimmed = scrubbed.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= MAX_EPISODE_DIGEST_BYTES {
        return Some(trimmed.to_string());
    }
    const SEPARATOR: &str = "\n\n...[truncated]...\n\n";
    let half = (MAX_EPISODE_DIGEST_BYTES.saturating_sub(SEPARATOR.len())) / 2;
    let head = truncate_utf8(trimmed, half);
    let mut tail_start = trimmed.len().saturating_sub(half);
    while tail_start < trimmed.len() && !trimmed.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail = &trimmed[tail_start..];
    Some(format!("{head}{SEPARATOR}{tail}"))
}

fn sanitize_escalation_reason(fallback: &str, prompt: Option<&str>) -> String {
    if let Some(prompt) = prompt {
        let single_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        let scrubbed = reiclaw_providers::scrub_secret_patterns(&single_line);
        let trimmed = scrubbed.trim();
        if !trimmed.is_empty() && find_protected_identity_marker(trimmed).is_none() {
            let excerpt = truncate_utf8(trimmed, MAX_REASON_EXCERPT_BYTES);
            return if excerpt.len() < trimmed.len() {
                format!("{excerpt}...")
            } else {
                excerpt.to_string()
            };
        }
    }
    fallback.to_string()
}

fn record_proxy_continuation_best_effort(
    data_dir: &Path,
    rowid: i64,
    revision: u64,
    new_count: u8,
) {
    let result = SqliteSessionBackend::new(data_dir)
        .and_then(|sessions| sessions.record_proxy_continuation(rowid, revision, new_count));
    if let Err(error) = result {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "rowid": rowid,
                    "revision": revision,
                    "error": reiclaw_providers::sanitize_api_error(&format!("{error:#}")),
                })),
            "failed to persist durable proxy continuation state"
        );
    }
}

async fn digest_terminal_result_fail_soft(
    agent_alias: &str,
    request_id: &str,
    dispatch_id: &str,
    status: &RunStatus,
    route_snapshot: &reiclaw_config::tachi::TachiConfig,
    deps: &SweepDeps<'_>,
) {
    let is_digestible_terminal = matches!(status.state, RunState::Completed)
        || (status.state == RunState::InputRequired
            && status.closure_kind.as_deref() == Some("partial"));
    if !is_digestible_terminal || status.result_written == Some(false) {
        return;
    }
    let Some(memory) = deps.memory else {
        return;
    };
    if memory.name() == "none" {
        return;
    }
    let Some(fetch_result) = deps.fetch_result else {
        return;
    };
    let result = match fetch_result(agent_alias, dispatch_id, route_snapshot).await {
        Ok(result) => result,
        Err(error) => {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "agent": agent_alias,
                        "request_id": request_id,
                        "dispatch_id": dispatch_id,
                        "error": reiclaw_providers::sanitize_api_error(&format!("{error:#}")),
                    })),
                "delegation terminal result fetch failed (fail-soft)"
            );
            return;
        }
    };
    let Some(summary) = result.body.as_deref().and_then(digest_result_body) else {
        return;
    };
    let ended_at = status
        .updated_at
        .as_deref()
        .filter(|raw| reiclaw_api::time::parse_rfc3339(raw).is_ok())
        .map(str::to_string)
        .unwrap_or_else(|| reiclaw_api::time::format_rfc3339(jiff::Timestamp::now()));
    let episode = EpisodeRecord {
        episode_id: format!("delegation:{request_id}"),
        agent_id: agent_alias.to_string(),
        session_key: request_id.to_string(),
        started_at: ended_at.clone(),
        ended_at,
        up_to_message_id: None,
        summary,
        why_and_tradeoffs: Vec::new(),
        settled_decisions: Vec::new(),
        rejected_options: Vec::new(),
        entities: Vec::new(),
        source_kind: EpisodeSourceKind::DelegationDigest,
    };
    if let Err(error) = memory.commit_episode(&episode).await {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "agent": agent_alias,
                    "request_id": request_id,
                    "episode_id": episode.episode_id,
                    "error": reiclaw_providers::sanitize_api_error(&format!("{error:#}")),
                })),
            "delegation terminal episode commit failed (fail-soft)"
        );
    }
}

struct TargetSnapshot {
    bridge: String,
    recipient: String,
    thread_id: Option<String>,
}

fn current_target_snapshot(
    config: &RwLock<Config>,
    data_dir: &Path,
    stop: &tokio_util::sync::CancellationToken,
    agent_alias: &str,
) -> Result<Option<TargetSnapshot>, bool> {
    let current = config.read();
    if stop.is_cancelled() || !eligible(&current, data_dir) {
        return Err(true);
    }
    if !current.agents.get(agent_alias).is_some_and(|a| a.enabled) {
        return Ok(None);
    }
    let Some(target) = current.tachi.result_notification.as_ref() else {
        return Err(true);
    };
    Ok(Some(TargetSnapshot {
        bridge: target.bridge.clone(),
        recipient: target.recipient.clone(),
        thread_id: target.thread_id.clone(),
    }))
}

#[allow(clippy::too_many_arguments)]
fn enqueue_deduped_notice(
    outbox: &BridgeOutbox,
    rowid: i64,
    agent_alias: &str,
    request_id: &str,
    notified_outcome: Option<&str>,
    target: &TargetSnapshot,
    outcome: &str,
    content: &str,
    marks: &mut Vec<MarkerUpdate>,
    notified: &mut usize,
) -> Result<(), anyhow::Error> {
    let thread = target.thread_id.as_deref();
    if notified_outcome.is_some_and(|list| {
        already_notified(list, outcome, &target.bridge, &target.recipient, thread)
    }) {
        return Ok(());
    }
    let source_id = source_identity(agent_alias, request_id);
    outbox.enqueue_source(
        &target.bridge,
        &target.recipient,
        thread,
        content,
        DELEGATION,
        &source_id,
        &destination_event_id(outcome, thread),
    )?;
    marks.push(MarkerUpdate {
        rowid,
        entry: marker_entry(outcome, &target.bridge, &target.recipient, thread),
        replaces: None,
    });
    *notified += 1;
    Ok(())
}

/// One reconcile pass: walk ONE bounded page of dispatch bindings past the
/// rotation cursor, and for each binding whose agent is enabled and whose
/// route provenance matches, ask the dispatch for its status (through the
/// SAME owned route snapshot the fingerprint was validated against — no
/// TOCTOU between validation and request), handle non-partial `INPUT_REQUIRED`
/// proxy continuations or owner escalations, enqueue terminal notices, and
/// digest terminal `result.md` reports into `EpisodeRecord`s fail-soft.
pub(crate) async fn reconcile_with_deps(
    config: &RwLock<Config>,
    data_dir: &Path,
    stop: &tokio_util::sync::CancellationToken,
    rotation: &mut Rotation,
    deps: &SweepDeps<'_>,
) -> anyhow::Result<usize> {
    if stop.is_cancelled() {
        return Ok(0);
    }
    if !eligible(&config.read(), data_dir) {
        return Ok(0);
    }
    // Scope the session handle: it must not live across the status awaits
    // below (keeps this future Send). The bounded page is fully owned; the
    // notified markers are written through one short-lived handle AFTER the
    // loop (batched, once per pass).
    let (page, table_extent) = {
        let sessions = SqliteSessionBackend::new(data_dir)?;
        let page = sessions.list_dispatch_bindings_page(rotation.cursor, rotation.page_size)?;
        (page, sessions.max_binding_rowid()?)
    };
    if page.len() < rotation.page_size {
        rotation.wrap();
    } else {
        if rotation.sweep_max_rowid == 0 {
            rotation.sweep_max_rowid = table_extent;
        }
        let churn_outran_sweep =
            page.last().expect("full page is nonempty").rowid > rotation.sweep_max_rowid;
        if churn_outran_sweep && rotation.full_pages_since_wrap >= REWIND_PASSES {
            // Forced rewind: insertions beyond the sweep's original extent
            // would otherwise postpone the natural wrap forever, and head
            // rows would never be RE-observed after completing.
            rotation.wrap();
        } else {
            rotation.cursor = page.last().expect("full page is nonempty").rowid;
            rotation.full_pages_since_wrap += 1;
        }
    }
    let outbox = BridgeOutbox::shared(data_dir)?;
    let mut notified = 0;
    let mut marks: Vec<MarkerUpdate> = Vec::new();
    let mut outbox_failure: Option<anyhow::Error> = None;
    for row in &page {
        if stop.is_cancelled() {
            break;
        }
        // Route provenance AND the request snapshot come from ONE owned
        // config read: the fingerprint is validated against this snapshot and
        // the same snapshot is handed to the fetcher, so a config swap mid-row
        // cannot validate on route A and query through route B.
        let (agent_enabled, route_snapshot, route_matches) = {
            let current = config.read();
            let enabled = current
                .agents
                .get(&row.agent_alias)
                .is_some_and(|a| a.enabled);
            let snapshot = current.tachi.clone();
            let fingerprint =
                reiclaw_tools::tachi_delegation::route_fingerprint(&snapshot, &row.agent_alias);
            (
                enabled,
                snapshot,
                row.route_digest.as_deref() == Some(fingerprint.as_str()),
            )
        };
        if !agent_enabled || !route_matches {
            continue;
        }
        // A failing status query is an UNKNOWN outcome, not a notification:
        // the next pass retries and only a terminal receipt ever enqueues.
        let Ok(status) =
            (deps.fetch_status)(&row.agent_alias, &row.dispatch_id, &route_snapshot).await
        else {
            continue;
        };

        if status.state == RunState::InputRequired
            && status.closure_kind.as_deref() != Some("partial")
        {
            let prompt = extract_input_required_prompt(&status);
            let gate =
                ProxyContinuationGate::with_prior_count(&row.dispatch_id, row.proxy_continuations);
            let escalation_reason = match gate.evaluate(&status) {
                Err(ProxyContinuationError::AuthRequired) => Some("AUTH_REQUIRED".to_string()),
                Err(ProxyContinuationError::CapExceeded { prior, max }) => {
                    Some(format!("proxy continuation cap reached ({prior}/{max})"))
                }
                Err(ProxyContinuationError::MissingRevision) => {
                    Some("missing status revision".to_string())
                }
                Err(_) => Some("proxy continuation refused".to_string()),
                Ok(revision) if row.last_proxy_revision == Some(revision) => Some(
                    sanitize_escalation_reason("awaiting owner confirmation", prompt.as_deref()),
                ),
                Ok(revision) => {
                    if let (Some(resolver), Some(send_continue)) =
                        (deps.continuation_resolver, deps.send_continue)
                    {
                        let ctx = ContinuationContext {
                            agent_alias: &row.agent_alias,
                            request_id: &row.request_id,
                            dispatch_id: &row.dispatch_id,
                            revision,
                            prompt: prompt.as_deref(),
                            read_projection: status.read_projection.as_ref(),
                        };
                        match resolver.resolve(&ctx).await {
                            Ok(Some(answer)) => match gate.admit(&status, &answer) {
                                Ok(expected_rev) => {
                                    match send_continue(
                                        &row.agent_alias,
                                        &row.dispatch_id,
                                        &route_snapshot,
                                        expected_rev,
                                        answer.trim(),
                                    )
                                    .await
                                    {
                                        Ok(_receipt) => {
                                            let new_count =
                                                row.proxy_continuations.saturating_add(1);
                                            record_proxy_continuation_best_effort(
                                                data_dir,
                                                row.rowid,
                                                expected_rev,
                                                new_count,
                                            );
                                            let target = match current_target_snapshot(
                                                config,
                                                data_dir,
                                                stop,
                                                &row.agent_alias,
                                            ) {
                                                Ok(Some(target)) => target,
                                                Ok(None) => continue,
                                                Err(_) => break,
                                            };
                                            let audit_outcome =
                                                format!("proxy_continued:{expected_rev}");
                                            let round_str = new_count.to_string();
                                            let max_str =
                                                MAX_PROXY_CONTINUATIONS_PER_DISPATCH.to_string();
                                            let content = reiclaw_runtime::i18n::get_required_cli_string_with_args(
                                                "delegation-proxy-continued-notice",
                                                &[
                                                    ("request", short(&row.request_id)),
                                                    ("dispatch", short(&row.dispatch_id)),
                                                    ("round", &round_str),
                                                    ("max", &max_str),
                                                ],
                                            );
                                            if let Err(error) = enqueue_deduped_notice(
                                                &outbox,
                                                row.rowid,
                                                &row.agent_alias,
                                                &row.request_id,
                                                row.notified_outcome.as_deref(),
                                                &target,
                                                &audit_outcome,
                                                &content,
                                                &mut marks,
                                                &mut notified,
                                            ) {
                                                outbox_failure = Some(error);
                                                break;
                                            }
                                            continue;
                                        }
                                        Err(TachiStaffError::SubmissionUnknown(_)) => {
                                            let new_count =
                                                row.proxy_continuations.saturating_add(1);
                                            record_proxy_continuation_best_effort(
                                                data_dir,
                                                row.rowid,
                                                expected_rev,
                                                new_count,
                                            );
                                            Some(
                                                "continuation submission outcome unknown"
                                                    .to_string(),
                                            )
                                        }
                                        Err(TachiStaffError::StaleRevision { .. }) => {
                                            continue;
                                        }
                                        Err(error) => {
                                            ::reiclaw_log::record!(
                                                WARN,
                                                ::reiclaw_log::Event::new(
                                                    module_path!(),
                                                    ::reiclaw_log::Action::Fail,
                                                )
                                                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                                                .with_attrs(::serde_json::json!({
                                                    "dispatch_id": row.dispatch_id,
                                                    "error": reiclaw_providers::sanitize_api_error(&error.to_string()),
                                                })),
                                                "proxy continuation call failed; escalating to owner"
                                            );
                                            Some(sanitize_escalation_reason(
                                                "automatic continuation refused",
                                                prompt.as_deref(),
                                            ))
                                        }
                                    }
                                }
                                Err(_) => Some(sanitize_escalation_reason(
                                    "proposed continuation rejected by safety gate",
                                    prompt.as_deref(),
                                )),
                            },
                            Ok(None) => Some(sanitize_escalation_reason(
                                "insufficient grounded context",
                                prompt.as_deref(),
                            )),
                            Err(error) => {
                                ::reiclaw_log::record!(
                                    WARN,
                                    ::reiclaw_log::Event::new(
                                        module_path!(),
                                        ::reiclaw_log::Action::Fail,
                                    )
                                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                                    .with_attrs(::serde_json::json!({
                                        "dispatch_id": row.dispatch_id,
                                        "error": reiclaw_providers::sanitize_api_error(&format!("{error:#}")),
                                    })),
                                    "proxy continuation resolver failed; escalating to owner"
                                );
                                Some(sanitize_escalation_reason(
                                    "awaiting owner input",
                                    prompt.as_deref(),
                                ))
                            }
                        }
                    } else {
                        Some(sanitize_escalation_reason(
                            "awaiting owner input",
                            prompt.as_deref(),
                        ))
                    }
                }
            };

            if let Some(reason) = escalation_reason {
                let target = match current_target_snapshot(config, data_dir, stop, &row.agent_alias)
                {
                    Ok(Some(target)) => target,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let content = reiclaw_runtime::i18n::get_required_cli_string_with_args(
                    "delegation-input-required-notice",
                    &[
                        ("request", short(&row.request_id)),
                        ("dispatch", short(&row.dispatch_id)),
                        ("reason", &reason),
                    ],
                );
                if let Err(error) = enqueue_deduped_notice(
                    &outbox,
                    row.rowid,
                    &row.agent_alias,
                    &row.request_id,
                    row.notified_outcome.as_deref(),
                    &target,
                    INPUT_REQUIRED_OUTCOME,
                    &content,
                    &mut marks,
                    &mut notified,
                ) {
                    outbox_failure = Some(error);
                    break;
                }
            }
            continue;
        }

        let Some(outcome) = terminal_outcome(&status) else {
            continue;
        };
        // Hold the current destination through enqueue: a simultaneous config
        // swap cannot send to a superseded target. The owning agent's
        // eligibility is re-read AFTER the await so a mid-flight disable
        // withholds too.
        let target = match current_target_snapshot(config, data_dir, stop, &row.agent_alias) {
            Ok(Some(target)) => target,
            Ok(None) => continue,
            Err(_) => break,
        };
        // Destination-scoped suppression: skip only when THIS outcome was
        // already notified to THIS destination — bridge, recipient AND
        // thread, the same triple permits_delivery gates on (across the whole remembered
        // target history, so A→B→A stays silent after retention eviction).
        let thread = target.thread_id.as_deref();
        let entry = marker_entry(outcome, &target.bridge, &target.recipient, thread);
        if row.notified_outcome.as_deref().is_some_and(|list| {
            already_notified(list, outcome, &target.bridge, &target.recipient, thread)
        }) {
            continue;
        }
        let source_id = source_identity(&row.agent_alias, &row.request_id);
        let legacy_entry = legacy_marker_entry(outcome, &target.bridge, &target.recipient);
        // The old producer marked at enqueue, not delivery, and used a bare
        // outcome event id even for threaded rows. Recover the old destination
        // from that receipt before interpreting its thread-blind marker. This
        // also handles a crash after enqueue but before the marker was saved.
        let legacy_receipt = match outbox.source_event(
            &target.bridge,
            &target.recipient,
            DELEGATION,
            &source_id,
            outcome,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                outbox_failure = Some(error);
                break;
            }
        };
        if let Some(receipt) = legacy_receipt {
            marks.push(MarkerUpdate {
                rowid: row.rowid,
                entry: marker_entry(
                    outcome,
                    &target.bridge,
                    &target.recipient,
                    receipt.thread_id.as_deref(),
                ),
                replaces: Some(legacy_entry),
            });
            if receipt.thread_id.as_deref() == thread {
                // Reuse the original candidate/receipt. In particular, an
                // uncertain or sent attempt must never be automatically replayed.
                continue;
            }
            // A different configured thread is a new destination. Leave the
            // original row and its owner actions untouched, as for new-format
            // A→B retargeting; only the new destination may get a new row.
        } else if row
            .notified_outcome
            .as_deref()
            .is_some_and(|list| marker_contains(list, &legacy_entry))
        {
            // Retention has erased the evidence needed to identify the old
            // thread. Do not guess that an old notification was never attempted.
            continue;
        }
        let content = reiclaw_runtime::i18n::get_required_cli_string_with_args(
            "delegation-result-notice",
            &[
                ("request", short(&row.request_id)),
                ("outcome", outcome),
                ("dispatch", short(&row.dispatch_id)),
            ],
        );
        // An enqueue failure (e.g. outbox capacity) must not discard the
        // markers of rows already enqueued this pass: record the error, stop
        // enqueueing further rows, and still flush what succeeded.
        if let Err(error) = outbox.enqueue_source(
            &target.bridge,
            &target.recipient,
            thread,
            &content,
            DELEGATION,
            &source_id,
            &destination_event_id(outcome, thread),
        ) {
            outbox_failure = Some(error);
            break;
        }
        marks.push(MarkerUpdate {
            rowid: row.rowid,
            entry,
            replaces: None,
        });
        notified += 1;

        // Fail-soft terminal result digestion into EpisodeRecord: runs after
        // the #63 BridgeOutbox notice is safely enqueued so any failure in
        // result fetch or episode commit never suppresses or delays delivery.
        digest_terminal_result_fail_soft(
            &row.agent_alias,
            &row.request_id,
            &row.dispatch_id,
            &status,
            &route_snapshot,
            deps,
        )
        .await;
    }
    // Batched marker persistence: one short-lived handle per pass, after the
    // awaits — reached on success, cancellation AND enqueue failure alike.
    // A crash between enqueue and this write is bounded by the outbox dedup
    // window (documented residual: at most one re-notification per outcome
    // after retention eviction, requiring a crash in that window).
    if !marks.is_empty() {
        let sessions = SqliteSessionBackend::new(data_dir)?;
        for update in &marks {
            // Per-row read with a propagated error: treating a read failure
            // as empty history would silently drop remembered destinations.
            let existing = sessions.binding_notified_marker(update.rowid)?;
            let retained = existing
                .as_deref()
                .unwrap_or("")
                .split('\u{1e}')
                .filter(|entry| Some(*entry) != update.replaces.as_deref())
                .collect::<Vec<_>>()
                .join("\u{1e}");
            let merged = marker_push(&retained, &update.entry);
            sessions.mark_binding_notified(update.rowid, &merged)?;
        }
    }
    if let Some(error) = outbox_failure {
        return Err(error);
    }
    Ok(notified)
}

/// Compatibility wrapper around [`reconcile_with_deps`] for status-only passes.
#[cfg(test)]
pub(crate) async fn reconcile(
    config: &RwLock<Config>,
    data_dir: &Path,
    stop: &tokio_util::sync::CancellationToken,
    rotation: &mut Rotation,
    fetch_status: StatusFetch<'_>,
) -> anyhow::Result<usize> {
    let deps = SweepDeps {
        fetch_status,
        fetch_result: None,
        send_continue: None,
        continuation_resolver: None,
        memory: None,
    };
    reconcile_with_deps(config, data_dir, stop, rotation, &deps).await
}

/// Delegation notices deliver only while this opt-in destination still
/// matches; queued-but-not-yet-sent rows are withheld when the target, the
/// owning agent's enablement, or the section disappears. Mirrors the review
/// gate. The owning agent is parsed from the row's agent-scoped source
/// identity — never the request id.
pub(crate) fn permits_delivery(
    config: &Config,
    outbox: &BridgeOutbox,
    row: &reiclaw_infra::bridge_outbox::OutboxItem,
) -> bool {
    if row.source_kind != DELEGATION {
        return true;
    }
    outbox.is_for_data_dir(&config.data_dir)
        && config.tachi.enabled
        && config
            .agents
            .get(owning_agent(&row.source_id))
            .is_some_and(|agent| agent.enabled)
        && config
            .tachi
            .result_notification
            .as_ref()
            .is_some_and(|target| {
                target.validate().is_ok()
                    && config.gateway.bridges.contains_key(&target.bridge)
                    && target.bridge == row.bridge
                    && target.recipient == row.to
                    && target.thread_id == row.thread_id
            })
}

pub(crate) struct DelegationNotificationTask {
    handle: tokio::task::JoinHandle<()>,
    wake: Arc<tokio::sync::Notify>,
}

impl DelegationNotificationTask {
    /// Nudge the delegation observer loop to run an immediate pass.
    #[allow(dead_code)]
    pub(crate) fn notify(&self) {
        self.wake.notify_one();
    }

    /// Clone the wake handle for push/SSE or in-process triggers.
    #[allow(dead_code)]
    pub(crate) fn wake_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.wake)
    }
}

impl Drop for DelegationNotificationTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Owned by the gateway serve future; cancelled on shutdown. One notice per
/// canonical (agent, request, outcome); delivery retries are the outbox's.
pub(crate) fn start(
    config: Arc<RwLock<Config>>,
    memory: Option<Arc<dyn Memory>>,
    wake: Option<Arc<tokio::sync::Notify>>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> DelegationNotificationTask {
    start_with_resolver(config, memory, wake, None, shutdown)
}

pub(crate) fn start_with_resolver(
    config: Arc<RwLock<Config>>,
    memory: Option<Arc<dyn Memory>>,
    wake: Option<Arc<tokio::sync::Notify>>,
    resolver: Option<Arc<dyn ContinuationResolver>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> DelegationNotificationTask {
    let data_dir: PathBuf = config.read().data_dir.clone();
    let wake = wake.unwrap_or_else(|| Arc::new(tokio::sync::Notify::new()));
    let wake_for_task = Arc::clone(&wake);
    let stop = tokio_util::sync::CancellationToken::new();
    let stop_on_drop = stop.clone().drop_guard();
    let handle = ::reiclaw_spawn::spawn!(async move {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the t=0 immediate tick so the initial pass runs once and then
        // awaits either the 60s interval, a wake nudge, or shutdown.
        interval.tick().await;
        let mut rotation = Rotation::default();
        loop {
            if *shutdown.borrow() {
                break;
            }
            // The live adapters: clients built per call from the VALIDATED route
            // snapshot handed in by reconcile_with_deps (never a fresh config
            // read — that would reintroduce the validate/query race).
            let fetch_status =
                |agent: &str, dispatch: &str, route: &reiclaw_config::tachi::TachiConfig| {
                    let route = route.clone();
                    let agent = agent.to_string();
                    let dispatch = dispatch.to_string();
                    Box::pin(async move {
                        let client = reiclaw_tools::tachi_staff::TachiStaffClient::from_config(
                            &route, &agent,
                        )?;
                        client.status(&dispatch).await.map_err(anyhow::Error::from)
                    }) as BoxFuture<'static, anyhow::Result<RunStatus>>
                };
            let fetch_result =
                |agent: &str, dispatch: &str, route: &reiclaw_config::tachi::TachiConfig| {
                    let route = route.clone();
                    let agent = agent.to_string();
                    let dispatch = dispatch.to_string();
                    Box::pin(async move {
                        let client = reiclaw_tools::tachi_staff::TachiStaffClient::from_config(
                            &route, &agent,
                        )?;
                        client.result(&dispatch).await.map_err(anyhow::Error::from)
                    }) as BoxFuture<'static, anyhow::Result<RunResult>>
                };
            let send_continue = |agent: &str,
                                 dispatch: &str,
                                 route: &reiclaw_config::tachi::TachiConfig,
                                 expected_rev: u64,
                                 answer: &str| {
                let route = route.clone();
                let agent = agent.to_string();
                let dispatch = dispatch.to_string();
                let answer = answer.to_string();
                Box::pin(async move {
                    let client =
                        reiclaw_tools::tachi_staff::TachiStaffClient::from_config(&route, &agent)?;
                    client.continue_run(&dispatch, expected_rev, &answer).await
                }) as BoxFuture<'static, Result<ContinueReceipt, TachiStaffError>>
            };
            let deps = SweepDeps {
                fetch_status: &fetch_status,
                fetch_result: Some(&fetch_result),
                send_continue: Some(&send_continue),
                continuation_resolver: resolver.as_deref(),
                memory: memory.as_deref(),
            };
            if let Err(error) =
                reconcile_with_deps(&config, &data_dir, &stop, &mut rotation, &deps).await
            {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{error:#}")})),
                    "delegation result observation pass failed"
                );
            }
            tokio::select! {
                _ = interval.tick() => {}
                _ = wake_for_task.notified() => {}
                _ = shutdown.changed() => break,
            }
        }
        drop(stop_on_drop);
    });
    DelegationNotificationTask { handle, wake }
}

#[cfg(test)]
mod tests;
