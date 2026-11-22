//! Typed sleep-memory records and invariants for ADR-025.
//!
//! These structs define the durable Episode, PinnedDigest, and DailyDigest
//! schemas shared between sleep-time memory compilation, the [`crate::memory_traits::Memory`]
//! trait boundary, and context assembly.

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

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

/// Stable action-candidate entry carried on a [`DailyDigest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionCandidateItem {
    pub id: String,
    pub text: String,
}

const fn default_daily_digest_revision() -> u64 {
    1
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
    #[serde(default = "default_daily_digest_revision")]
    pub revision: u64,
    #[serde(default)]
    pub candidate_items: Vec<ActionCandidateItem>,
}

impl DailyDigest {
    /// Compute a deterministic stable identifier (`cand_<16-hex>`) for a legacy
    /// string action candidate so upgrades remain stable and edits do not mint
    /// duplicate candidate identities.
    #[must_use]
    pub fn deterministic_legacy_candidate_id(
        agent_id: &str,
        day_key: &str,
        index: usize,
        text: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"reiclaw.daily_digest.candidate.v1\0");
        hasher.update(agent_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(day_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(index.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(text.trim().as_bytes());
        let digest = hasher.finalize();
        let mut id = String::with_capacity(21);
        id.push_str("cand_");
        for byte in &digest[..8] {
            let _ = write!(id, "{byte:02x}");
        }
        id
    }

    /// Return the canonical action candidates with stable IDs. Uses stored
    /// [`Self::candidate_items`] when present, otherwise deterministically
    /// derives legacy IDs from [`Self::action_candidates`].
    #[must_use]
    pub fn normalized_candidates(&self) -> Vec<ActionCandidateItem> {
        if !self.candidate_items.is_empty() {
            return self.candidate_items.clone();
        }
        self.action_candidates
            .iter()
            .enumerate()
            .map(|(index, text)| ActionCandidateItem {
                id: Self::deterministic_legacy_candidate_id(
                    &self.agent_id,
                    &self.day_key,
                    index,
                    text,
                ),
                text: text.clone(),
            })
            .collect()
    }

    /// Populate [`Self::candidate_items`] from [`Self::action_candidates`] when
    /// missing, without regenerating IDs that already exist.
    pub fn ensure_candidate_items(&mut self) {
        if self.candidate_items.is_empty() && !self.action_candidates.is_empty() {
            self.candidate_items = self.normalized_candidates();
        }
    }

    /// Resolve the current decision status for one normalized candidate item.
    /// Explicit ID-keyed decisions take precedence; legacy text-keyed entries
    /// only match when `item.text` is unambiguous across normalized candidates.
    #[must_use]
    pub fn candidate_status(&self, item: &ActionCandidateItem) -> ActionCandidateStatus {
        if let Some(status) = self.action_decisions.get(&item.id) {
            return *status;
        }
        let matching_text_count = self
            .normalized_candidates()
            .into_iter()
            .filter(|candidate| candidate.text == item.text)
            .count();
        if matching_text_count == 1
            && let Some(status) = self.action_decisions.get(&item.text)
        {
            return *status;
        }
        ActionCandidateStatus::Pending
    }

    /// Look up one normalized candidate unambiguously by legacy display text.
    /// Returns `Ok(None)` when absent, `Err` when multiple candidates share
    /// the same trimmed text.
    pub fn find_candidate_by_text(
        &self,
        text: &str,
    ) -> Result<Option<ActionCandidateItem>, &'static str> {
        let trimmed = text.trim();
        let mut matches: Vec<ActionCandidateItem> = self
            .normalized_candidates()
            .into_iter()
            .filter(|item| item.text.trim() == trimmed)
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches.remove(0))),
            _ => Err("ambiguous action candidate text; specify candidate_id"),
        }
    }

    /// Validate day key, agent scope, revision, candidate IDs, narrative, and RFC 3339 timestamps.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_agent_id(&self.agent_id)?;
        validate_non_soul_key(&self.day_key, "day_key")?;
        ensure!(self.revision >= 1, "daily digest revision must be >= 1");
        ensure!(
            !self.narrative.trim().is_empty(),
            "daily digest narrative must not be empty"
        );
        let mut seen_ids = HashSet::with_capacity(self.candidate_items.len());
        for item in &self.candidate_items {
            validate_candidate_id(&item.id)?;
            ensure!(
                !item.text.trim().is_empty(),
                "daily digest candidate item '{}' must have non-empty text",
                item.id
            );
            ensure!(
                seen_ids.insert(item.id.as_str()),
                "duplicate daily digest candidate item id '{}'",
                item.id
            );
        }
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

/// Validate a stable [`ActionCandidateItem`] identifier.
pub fn validate_candidate_id(id: &str) -> anyhow::Result<()> {
    validate_non_soul_key(id, "candidate_id")?;
    ensure!(
        id == id.trim() && id.len() <= 128,
        "candidate_id must be trimmed and at most 128 bytes"
    );
    ensure!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "candidate_id '{id}' must contain only ASCII letters, digits, '-' or '_'"
    );
    Ok(())
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
            revision: 1,
            candidate_items: vec![],
        };

        let json = serde_json::to_string(&digest).unwrap();
        assert!(json.contains("\"pending\""));
        assert!(json.contains("\"approved\""));
        assert!(json.contains("\"dismissed\""));
        let parsed: DailyDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, digest);
    }
}
