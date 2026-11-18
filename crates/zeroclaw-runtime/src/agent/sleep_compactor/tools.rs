//! Confined sleep-time primitives (`Tool` implementations) for ADR-025.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use zeroclaw_api::attribution::ToolKind;
use zeroclaw_api::memory_traits::{
    ActionCandidateStatus, DailyDigest, EpisodeRecord, EpisodeSourceKind, Memory, MemoryCategory,
    PINNED_DIGEST_MAX_BYTES, PinnedDigest, validate_agent_id, validate_non_soul_key,
    validate_non_soul_namespace,
};
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::schema::MemoryConfig;
use zeroclaw_infra::session_backend::SessionBackend;

use crate::agent::history::floor_char_boundary;

use super::{
    ExtractedCoreFact, SharedOutcome, is_unsupported_sleep_error, persist_extracted_core_facts,
    string_array_arg,
};
// ── Confined Sleep Primitives (7 `Tool` implementations) ─────────────────────

pub(super) struct SessionSearchTool {
    pub(super) session_backend: Arc<dyn SessionBackend>,
    pub(super) allowed_session_keys: Vec<String>,
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

pub(super) struct EpisodeReadTool {
    pub(super) memory: Arc<dyn Memory>,
    pub(super) agent_id: String,
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

pub(super) struct CheckpointStageTool {
    pub(super) session_backend: Arc<dyn SessionBackend>,
    pub(super) session_key: String,
    pub(super) max_message_id: i64,
    pub(super) outcome: SharedOutcome,
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

pub(super) struct EpisodeCommitTool {
    pub(super) memory: Arc<dyn Memory>,
    pub(super) memory_config: MemoryConfig,
    pub(super) agent_id: String,
    pub(super) default_session_key: String,
    pub(super) source_kind: EpisodeSourceKind,
    pub(super) default_started_at: String,
    pub(super) default_ended_at: String,
    pub(super) default_up_to_message_id: Option<i64>,
    pub(super) tolerate_unsupported_sleep: bool,
    pub(super) outcome: SharedOutcome,
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

pub(super) struct MemorySupersedeTool {
    pub(super) memory: Arc<dyn Memory>,
    pub(super) agent_id: String,
    pub(super) outcome: SharedOutcome,
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

pub(super) struct ActionProposeTool {
    pub(super) outcome: SharedOutcome,
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

pub(super) struct DigestPublishTool {
    pub(super) memory: Arc<dyn Memory>,
    pub(super) agent_id: String,
    pub(super) day_key: String,
    pub(super) window_start: String,
    pub(super) window_end: String,
    pub(super) outcome: SharedOutcome,
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
pub(super) async fn publish_dream_digests(
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
