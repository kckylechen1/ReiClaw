//! Typed sleep-memory records and invariants for ADR-025.
//!
//! These structs define the durable Episode, PinnedDigest, and DailyDigest
//! schemas shared between sleep-time memory compilation, the [`crate::memory_traits::Memory`]
//! trait boundary, and context assembly.

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Maximum UTF-8 byte length permitted for [`PinnedDigest::rendered_index`].
pub const PINNED_DIGEST_MAX_BYTES: usize = 8_000;

/// Reserved Soul namespace/prefix names checked at record validation boundaries.
const SOUL_NAMESPACE: &str = "soul";
const SOUL_KEY_PREFIX: &str = "soul::";

/// Provenance of a committed [`EpisodeRecord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeSourceKind {
    #[default]
    Dialogue,
    DelegationDigest,
    IdleFlush,
    StartupRecovery,
}

impl EpisodeSourceKind {
    /// Canonical snake_case representation for metadata and indexing.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dialogue => "dialogue",
            Self::DelegationDigest => "delegation_digest",
            Self::IdleFlush => "idle_flush",
            Self::StartupRecovery => "startup_recovery",
        }
    }
}

impl std::fmt::Display for EpisodeSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Durable cross-session episode summary committed during background compaction
/// or idle session flush.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeRecord {
    pub episode_id: String,
    pub agent_id: String,
    pub session_key: String,
    pub started_at: String,
    pub ended_at: String,
    #[serde(default)]
    pub up_to_message_id: Option<i64>,
    pub summary: String,
    #[serde(default)]
    pub why_and_tradeoffs: Vec<String>,
    #[serde(default)]
    pub settled_decisions: Vec<String>,
    #[serde(default)]
    pub rejected_options: Vec<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub source_kind: EpisodeSourceKind,
}

impl EpisodeRecord {
    /// Validate required fields, RFC 3339 timestamps, and Soul namespace isolation.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_agent_id(&self.agent_id)?;
        validate_non_soul_key(&self.episode_id, "episode_id")?;
        validate_non_soul_key(&self.session_key, "session_key")?;
        ensure!(
            !self.summary.trim().is_empty(),
            "episode summary must not be empty"
        );
        let start = crate::time::parse_rfc3339(&self.started_at)
            .with_context(|| format!("invalid episode started_at '{}'", self.started_at))?;
        let end = crate::time::parse_rfc3339(&self.ended_at)
            .with_context(|| format!("invalid episode ended_at '{}'", self.ended_at))?;
        ensure!(
            end >= start,
            "episode ended_at ({}) must not precede started_at ({})",
            self.ended_at,
            self.started_at
        );
        Ok(())
    }
}

/// Compact cache-pinned index summarizing active projects, settled decisions,
/// and entity anchors for L0 prefix injection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedDigest {
    pub agent_id: String,
    pub rendered_index: String,
    pub updated_at: String,
    #[serde(default)]
    pub source_day_key: Option<String>,
}

impl PinnedDigest {
    /// Validate agent scope, UTF-8 byte budget, and timestamp format.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_agent_id(&self.agent_id)?;
        let byte_len = self.rendered_index.len();
        ensure!(
            byte_len <= PINNED_DIGEST_MAX_BYTES,
            "pinned digest rendered_index exceeds {PINNED_DIGEST_MAX_BYTES} UTF-8 bytes ({byte_len} bytes)"
        );
        crate::time::parse_rfc3339(&self.updated_at)
            .with_context(|| format!("invalid pinned digest updated_at '{}'", self.updated_at))?;
        if let Some(day_key) = &self.source_day_key {
            validate_non_soul_key(day_key, "source_day_key")?;
        }
        Ok(())
    }
}

/// Review decision status for an action candidate proposed in a [`DailyDigest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCandidateStatus {
    #[default]
    Pending,
    Approved,
    Dismissed,
}

/// Structured daily synthesis produced by the deep-sleep consolidation pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyDigest {
    pub day_key: String,
    pub agent_id: String,
    pub window_start: String,
    pub window_end: String,
    pub narrative: String,
    #[serde(default)]
    pub settled_decisions: Vec<String>,
    #[serde(default)]
    pub why_and_tradeoffs: Vec<String>,
    #[serde(default)]
    pub latent_insights: Vec<String>,
    #[serde(default)]
    pub action_candidates: Vec<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    pub created_at: String,
    #[serde(default)]
    pub owner_edited: bool,
    #[serde(default)]
    pub action_decisions: BTreeMap<String, ActionCandidateStatus>,
}

impl DailyDigest {
    /// Validate day key, agent scope, narrative, and RFC 3339 timestamps.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_agent_id(&self.agent_id)?;
        validate_non_soul_key(&self.day_key, "day_key")?;
        ensure!(
            !self.narrative.trim().is_empty(),
            "daily digest narrative must not be empty"
        );
        let start = crate::time::parse_rfc3339(&self.window_start).with_context(|| {
            format!("invalid daily digest window_start '{}'", self.window_start)
        })?;
        let end = crate::time::parse_rfc3339(&self.window_end)
            .with_context(|| format!("invalid daily digest window_end '{}'", self.window_end))?;
        ensure!(
            end >= start,
            "daily digest window_end ({}) must not precede window_start ({})",
            self.window_end,
            self.window_start
        );
        crate::time::parse_rfc3339(&self.created_at)
            .with_context(|| format!("invalid daily digest created_at '{}'", self.created_at))?;
        Ok(())
    }
}

/// Validate that `agent_id` is non-empty and does not target the reserved Soul namespace.
pub fn validate_agent_id(agent_id: &str) -> anyhow::Result<()> {
    let trimmed = agent_id.trim();
    ensure!(!trimmed.is_empty(), "agent_id must not be empty");
    if trimmed == SOUL_NAMESPACE
        || agent_id == SOUL_NAMESPACE
        || agent_id.starts_with(SOUL_KEY_PREFIX)
    {
        bail!("refused: agent_id '{agent_id}' targets reserved Soul namespace");
    }
    Ok(())
}

/// Validate that `key` (labelled by `field_name`) is non-empty and outside the Soul key space.
pub fn validate_non_soul_key(key: &str, field_name: &str) -> anyhow::Result<()> {
    ensure!(!key.trim().is_empty(), "{field_name} must not be empty");
    if key.starts_with(SOUL_KEY_PREFIX) || key.trim() == SOUL_NAMESPACE {
        bail!("refused: {field_name} '{key}' is reserved for the Soul namespace");
    }
    Ok(())
}

/// Validate that an optional `namespace` is non-empty and outside the Soul namespace.
pub fn validate_non_soul_namespace(namespace: Option<&str>) -> anyhow::Result<()> {
    if let Some(ns) = namespace {
        ensure!(!ns.trim().is_empty(), "namespace must not be empty");
        if ns.trim() == SOUL_NAMESPACE || ns.starts_with(SOUL_KEY_PREFIX) {
            bail!("refused: namespace '{ns}' is reserved for the Soul namespace");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_digest_defaults_owner_edited_and_action_decisions_when_absent() {
        let legacy_json = r#"{
            "day_key": "2026-11-18",
            "agent_id": "agent_a",
            "window_start": "2026-11-17T04:00:00Z",
            "window_end": "2026-11-18T04:00:00Z",
            "narrative": "Legacy digest without Wave 2 review fields",
            "created_at": "2026-11-18T04:01:00Z"
        }"#;

        let parsed: DailyDigest = serde_json::from_str(legacy_json).unwrap();
        assert!(!parsed.owner_edited);
        assert!(parsed.action_decisions.is_empty());
        assert_eq!(
            ActionCandidateStatus::default(),
            ActionCandidateStatus::Pending
        );
    }

    #[test]
    fn daily_digest_roundtrip_preserves_owner_edited_and_action_decisions() {
        let mut decisions = BTreeMap::new();
        decisions.insert("cand-1".to_string(), ActionCandidateStatus::Pending);
        decisions.insert("cand-2".to_string(), ActionCandidateStatus::Approved);
        decisions.insert("cand-3".to_string(), ActionCandidateStatus::Dismissed);

        let digest = DailyDigest {
            day_key: "2026-11-18".to_string(),
            agent_id: "agent_a".to_string(),
            window_start: "2026-11-17T04:00:00Z".to_string(),
            window_end: "2026-11-18T04:00:00Z".to_string(),
            narrative: "Reviewed digest".to_string(),
            settled_decisions: vec!["use trait boundary".to_string()],
            why_and_tradeoffs: vec![],
            latent_insights: vec![],
            action_candidates: vec!["cand-1".to_string(), "cand-2".to_string()],
            entities: vec!["zeroclaw".to_string()],
            created_at: "2026-11-18T04:01:00Z".to_string(),
            owner_edited: true,
            action_decisions: decisions,
        };

        let json = serde_json::to_string(&digest).unwrap();
        assert!(json.contains("\"pending\""));
        assert!(json.contains("\"approved\""));
        assert!(json.contains("\"dismissed\""));
        let parsed: DailyDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, digest);
    }
}
