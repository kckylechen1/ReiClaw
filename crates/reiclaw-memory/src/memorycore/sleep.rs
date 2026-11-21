//! Sleep-memory primitives over `MemoryCore`'s single `MemoryStore` handle
//! (ADR-025 Wave 1 Slice 2/3B).

use super::{
    DEFAULT_NAMESPACE, METADATA_PINNED, METADATA_SESSION_ID, METADATA_SUPERSEDED_BY,
    METADATA_ZC_AGENT, METADATA_ZC_CATEGORY, METADATA_ZC_CREATED_AT, METADATA_ZC_KEY,
    METADATA_ZC_NAMESPACE, METADATA_ZC_UPDATED_AT, MemoryCore,
};
use crate::point_scope::PointScope;
use crate::sleep_records::{
    DailyDigest, EpisodeRecord, PinnedDigest, validate_agent_id, validate_non_soul_key,
    validate_non_soul_namespace,
};
use crate::traits::{MemoryCategory, StoreOptions};
use anyhow::{Context, bail, ensure};
use portable_kernel::MemoryStore;
use portable_kernel::store::scoped_read::{MetadataTextFilter, RecentMemoryQuery, TextMembership};
use serde::Serialize;
use uuid::Uuid;

const SLEEP_EPISODE_NAMESPACE: &str = "sleep_episode";
const SLEEP_EPISODE_CATEGORY: &str = "sleep_episode";
const SLEEP_PINNED_DIGEST_NAMESPACE: &str = "sleep_pinned_digest";
const SLEEP_PINNED_DIGEST_CATEGORY: &str = "sleep_pinned_digest";
const SLEEP_PINNED_DIGEST_KEY: &str = "sleep:pinned_digest";
const SLEEP_DAILY_DIGEST_NAMESPACE: &str = "sleep_daily_digest";
const SLEEP_DAILY_DIGEST_CATEGORY: &str = "sleep_daily_digest";
const SLEEP_RECORD_IMPORTANCE: f64 = 0.8;
const CORE_FACT_IMPORTANCE: f64 = 0.7;

fn encode_sleep_json<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let raw = serde_json::to_string(value).context("failed to serialize sleep record")?;
    // Escape '<' so memcore's `scrub_think_tags` cannot alter serialized JSON strings.
    Ok(raw.replace('<', "\\u003c"))
}

fn ceil_utc_millis(ts: jiff::Timestamp) -> String {
    let nanos = ts.subsec_nanosecond();
    let adjusted = if nanos % 1_000_000 != 0 {
        ts.checked_add(jiff::SignedDuration::from_millis(1))
            .unwrap_or(ts)
    } else {
        ts
    };
    MemoryCore::format_utc_millis(adjusted)
}

fn validate_optional_session_id(session_id: Option<&str>) -> anyhow::Result<()> {
    if let Some(sid) = session_id {
        validate_non_soul_key(sid, "session_id")?;
    }
    Ok(())
}

impl MemoryCore {
    fn episode_category() -> MemoryCategory {
        MemoryCategory::Custom(SLEEP_EPISODE_CATEGORY.to_string())
    }

    fn pinned_digest_category() -> MemoryCategory {
        MemoryCategory::Custom(SLEEP_PINNED_DIGEST_CATEGORY.to_string())
    }

    fn daily_digest_category() -> MemoryCategory {
        MemoryCategory::Custom(SLEEP_DAILY_DIGEST_CATEGORY.to_string())
    }

    fn sleep_subtree_prefix(agent_id: &str, namespace: &str, category: &MemoryCategory) -> String {
        format!(
            "/agents/{}/{}/{}",
            Self::agent_segment(Some(agent_id)),
            Self::namespace_segment(Some(namespace)),
            Self::category_to_segment(category)
        )
    }

    fn resolve_existing_at_path(
        store: &MemoryStore,
        path: &str,
        key: &str,
        agent_id: &str,
        include_archived: bool,
    ) -> anyhow::Result<Option<portable_kernel::MemoryEntry>> {
        let rows = store
            .list_by_path(path, 8, include_archived)
            .map_err(|e| anyhow::Error::msg(format!("tachi sleep identity lookup failed: {e}")))?;
        Ok(rows.into_iter().find(|entry| {
            entry.path == path
                && Self::metadata_str(&entry.metadata, METADATA_ZC_KEY).as_deref() == Some(key)
                && Self::metadata_str(&entry.metadata, METADATA_ZC_AGENT).as_deref()
                    == Some(agent_id)
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_sleep_entry(
        existing: Option<&portable_kernel::MemoryEntry>,
        path: String,
        key: &str,
        category: &MemoryCategory,
        namespace: &str,
        agent_id: &str,
        session_id: Option<&str>,
        summary: String,
        text: String,
        entities: Vec<String>,
        keywords: Vec<String>,
        timestamp_millis: String,
        timestamp_nanos: String,
        pinned: bool,
        retention_policy: Option<&str>,
    ) -> portable_kernel::MemoryEntry {
        let id = existing
            .map(|e| e.id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let access_count = existing.map(|e| e.access_count).unwrap_or(0);
        let last_access = existing.and_then(|e| e.last_access.clone());
        let recall_count = existing.map(|e| e.recall_count).unwrap_or(0);
        let query_diversity = existing.map(|e| e.query_diversity).unwrap_or(0);
        let tier = existing
            .map(|e| e.tier.clone())
            .unwrap_or_else(portable_kernel::types::default_tier);
        let revision = existing.map(|e| e.revision).unwrap_or(1);

        let options = StoreOptions::default().pinned(pinned);
        let mut metadata = Self::build_metadata(
            key,
            category,
            session_id,
            Some(namespace),
            Some(agent_id),
            &options,
        );
        let created_at_nanos = existing
            .and_then(|e| {
                Self::metadata_str(&e.metadata, METADATA_ZC_CREATED_AT)
                    .filter(|s| !s.is_empty())
                    .or_else(|| Some(Self::canonical_sort_nanos(&e.timestamp)))
            })
            .unwrap_or_else(|| timestamp_nanos.clone());
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                METADATA_ZC_CREATED_AT.to_string(),
                serde_json::Value::String(created_at_nanos),
            );
            obj.insert(
                METADATA_ZC_UPDATED_AT.to_string(),
                serde_json::Value::String(timestamp_nanos),
            );
        }

        portable_kernel::MemoryEntry {
            id,
            path,
            summary,
            text,
            importance: SLEEP_RECORD_IMPORTANCE,
            timestamp: timestamp_millis.clone(),
            valid_from: timestamp_millis,
            valid_until: None,
            category: Self::category_to_memcore(category),
            topic: String::new(),
            keywords,
            persons: Vec::new(),
            entities,
            location: String::new(),
            source: "reiclaw".into(),
            scope: Self::scope_for_namespace(Some(namespace)),
            archived: false,
            access_count,
            scored_count: 0,
            last_use_at: None,
            last_access,
            revision,
            vector: None,
            retention_policy: retention_policy.map(str::to_string),
            domain: None,
            metadata,
            recall_count,
            query_diversity,
            tier,
        }
    }

    fn sleep_recent_query(
        prefix: String,
        agent_id: &str,
        namespace: &str,
        category: &str,
        session_id: Option<&str>,
        limit: usize,
    ) -> RecentMemoryQuery {
        let mut query = RecentMemoryQuery::new(prefix, limit);
        query.filters.push(MetadataTextFilter {
            field: METADATA_ZC_AGENT.into(),
            values: vec![agent_id.to_string()],
            membership: TextMembership::AnyOf,
            missing_value: None,
        });
        query.filters.push(MetadataTextFilter {
            field: METADATA_ZC_NAMESPACE.into(),
            values: vec![namespace.to_string()],
            membership: TextMembership::AnyOf,
            missing_value: None,
        });
        query.filters.push(MetadataTextFilter {
            field: METADATA_ZC_CATEGORY.into(),
            values: vec![category.to_string()],
            membership: TextMembership::AnyOf,
            missing_value: None,
        });
        query.filters.push(MetadataTextFilter {
            field: METADATA_SUPERSEDED_BY.into(),
            values: vec![String::new()],
            membership: TextMembership::AnyOf,
            missing_value: Some(String::new()),
        });
        if let Some(sid) = session_id {
            query.filters.push(MetadataTextFilter {
                field: METADATA_SESSION_ID.into(),
                values: vec![sid.to_string()],
                membership: TextMembership::AnyOf,
                missing_value: None,
            });
        }
        query
    }

    /// Persist an [`EpisodeRecord`] under this agent's dedicated non-soul
    /// episode namespace using the shared `MemoryStore` handle.
    pub async fn commit_episode(&self, episode: &EpisodeRecord) -> anyhow::Result<String> {
        episode.validate()?;
        let end_ts = reiclaw_api::time::parse_rfc3339(&episode.ended_at)?;
        let ts_millis = Self::format_utc_millis(end_ts);
        let ts_nanos = Self::format_utc_nanos(end_ts);
        let payload = encode_sleep_json(episode)?;
        let category = Self::episode_category();
        let path = Self::storage_path(
            Some(&episode.agent_id),
            Some(SLEEP_EPISODE_NAMESPACE),
            &category,
            &episode.episode_id,
        );
        let mut keywords = episode.entities.clone();
        keywords.push(episode.source_kind.to_string());
        let summary: String = episode.summary.chars().take(200).collect();

        let store = self.store_handle().clone();
        let episode_id = episode.episode_id.clone();
        let agent_id = episode.agent_id.clone();
        let session_key = episode.session_key.clone();
        let entities = episode.entities.clone();
        let immutable =
            episode.source_kind == crate::sleep_records::EpisodeSourceKind::DelegationDigest;

        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let mut store = store.lock();
            let existing =
                Self::resolve_existing_at_path(&store, &path, &episode_id, &agent_id, immutable)?;
            if immutable && let Some(existing) = &existing {
                let stored: EpisodeRecord = serde_json::from_str(&existing.text)?;
                stored.validate()?;
                ensure!(
                    stored.episode_id == episode_id
                        && stored.agent_id == agent_id
                        && stored.session_key == session_key
                        && stored.source_kind
                            == crate::sleep_records::EpisodeSourceKind::DelegationDigest,
                    "delegation Episode identity conflicts"
                );
                return Ok(existing.id.clone());
            }
            let entry = Self::build_sleep_entry(
                existing.as_ref(),
                path,
                &episode_id,
                &category,
                SLEEP_EPISODE_NAMESPACE,
                &agent_id,
                Some(&session_key),
                summary,
                payload,
                entities,
                keywords,
                ts_millis,
                ts_nanos,
                false,
                Some("durable"),
            );
            let id = entry.id.clone();
            store
                .upsert(&entry)
                .map_err(|e| anyhow::Error::msg(format!("tachi episode commit failed: {e}")))?;
            Ok(id)
        })
        .await?
    }

    /// Read non-superseded [`EpisodeRecord`]s for `agent_id` within `[since, until]`
    /// (inclusive on `ended_at`), optionally filtered to `session_id`, newest first.
    pub async fn read_episodes_in_window(
        &self,
        agent_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        session_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<EpisodeRecord>> {
        validate_agent_id(agent_id)?;
        validate_optional_session_id(session_id)?;

        let since_ts = since
            .map(reiclaw_api::time::parse_rfc3339)
            .transpose()
            .context("invalid since timestamp for read_episodes_in_window")?;
        let until_ts = until
            .map(reiclaw_api::time::parse_rfc3339)
            .transpose()
            .context("invalid until timestamp for read_episodes_in_window")?;

        if limit == 0 {
            return Ok(Vec::new());
        }
        if let (Some(s), Some(u)) = (since_ts, until_ts)
            && s > u
        {
            return Ok(Vec::new());
        }

        let category = Self::episode_category();
        let prefix = Self::sleep_subtree_prefix(agent_id, SLEEP_EPISODE_NAMESPACE, &category);
        let store = self.store_handle().clone();
        let max_grow = self.max_list_grow.max(1);
        let agent_owned = agent_id.to_string();
        let session_owned = session_id.map(str::to_owned);

        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<EpisodeRecord>> {
            let store = store.lock();
            let mut fetch_limit = limit.max(16).min(max_grow);
            loop {
                let mut query = Self::sleep_recent_query(
                    prefix.clone(),
                    &agent_owned,
                    SLEEP_EPISODE_NAMESPACE,
                    SLEEP_EPISODE_CATEGORY,
                    session_owned.as_deref(),
                    fetch_limit,
                );
                query.since = since_ts.map(Self::format_utc_millis);
                query.until = until_ts.map(ceil_utc_millis);

                let rows = store.list_recent_matching(&query).map_err(|e| {
                    anyhow::Error::msg(format!("tachi read_episodes_in_window failed: {e}"))
                })?;
                let raw_len = rows.len();

                let mut episodes = Vec::with_capacity(limit.min(raw_len));
                for row in rows {
                    if Self::is_soft_superseded(&row) {
                        continue;
                    }
                    let episode: EpisodeRecord = serde_json::from_str(&row.text)
                        .with_context(|| format!("failed to parse EpisodeRecord '{}'", row.id))?;
                    if episode.agent_id != agent_owned {
                        continue;
                    }
                    if let Some(sid) = session_owned.as_deref()
                        && episode.session_key != sid
                    {
                        continue;
                    }
                    let ended_ts = reiclaw_api::time::parse_rfc3339(&episode.ended_at)?;
                    if let Some(s) = since_ts
                        && ended_ts < s
                    {
                        continue;
                    }
                    if let Some(u) = until_ts
                        && ended_ts > u
                    {
                        continue;
                    }
                    episodes.push(episode);
                    if episodes.len() >= limit {
                        break;
                    }
                }

                if episodes.len() >= limit || raw_len < fetch_limit || fetch_limit >= max_grow {
                    return Ok(episodes);
                }
                fetch_limit = fetch_limit.saturating_mul(2).min(max_grow);
            }
        })
        .await?
    }

    /// Atomically insert/upsert a replacement `Core` entry for `new_key` and
    /// stamp `METADATA_SUPERSEDED_BY` on `old_key` within a single `store_handle()`
    /// lock and SQLite `IMMEDIATE` transaction.
    pub async fn supersede_fact(
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
        ensure!(
            old_key != new_key,
            "old_key and new_key must be distinct; re-storing the same key updates in place"
        );
        ensure!(
            !new_content.trim().is_empty(),
            "replacement content must not be empty"
        );

        let scope = PointScope::current();
        let embedding = self.compute_embedding(new_content).await;
        let now_dt = jiff::Timestamp::now();
        let now_millis = Self::format_utc_millis(now_dt);
        let now_nanos = Self::format_utc_nanos(now_dt);

        let store = self.store_handle().clone();
        let agent_owned = agent_id.to_string();
        let old_key_owned = old_key.to_string();
        let new_key_owned = new_key.to_string();
        let new_content_owned = new_content.to_string();
        let requested_ns = namespace.map(str::to_owned);

        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let mut store = store.lock();

            let old_candidates =
                Self::rows_matching_key(&store, &old_key_owned, Some(&agent_owned))?;
            let old_exact: Vec<portable_kernel::MemoryEntry> = old_candidates
                .into_iter()
                .filter(|row| {
                    Self::metadata_str(&row.metadata, METADATA_ZC_KEY).as_deref()
                        == Some(old_key_owned.as_str())
                        && Self::metadata_str(&row.metadata, METADATA_ZC_AGENT).as_deref()
                            == Some(agent_owned.as_str())
                })
                .collect();

            if old_exact.iter().any(|row| {
                Self::metadata_str(&row.metadata, METADATA_ZC_NAMESPACE).as_deref()
                    == Some(crate::SOUL_NAMESPACE)
            }) {
                bail!("refused: cannot supersede a row in the reserved Soul namespace");
            }

            let admitted_old_ids: std::collections::HashSet<String> = scope
                .select(
                    old_exact
                        .iter()
                        .cloned()
                        .map(|row| Self::memcore_to_zeroclaw(row, None))
                        .collect(),
                    false,
                )
                .into_iter()
                .map(|entry| entry.id)
                .collect();

            let scoped_old: Vec<portable_kernel::MemoryEntry> = old_exact
                .into_iter()
                .filter(|row| admitted_old_ids.contains(&row.id))
                .collect();

            if scoped_old.is_empty() {
                bail!("fact '{old_key_owned}' not found for agent '{agent_owned}'");
            }

            let Some(mut old_row) = scoped_old
                .into_iter()
                .rev()
                .find(|row| !Self::is_soft_superseded(row))
            else {
                bail!("fact '{old_key_owned}' for agent '{agent_owned}' is already superseded");
            };

            ensure!(
                !old_row.id.starts_with("wiki-rem:") && !old_row.id.starts_with("anchor:"),
                "tachi supersede_fact refused on reserved id '{}'",
                old_row.id
            );

            let old_ns = Self::metadata_str(&old_row.metadata, METADATA_ZC_NAMESPACE)
                .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
            let target_ns = requested_ns.as_deref().unwrap_or(old_ns.as_str());
            validate_non_soul_namespace(Some(target_ns))?;

            let new_category = MemoryCategory::Core;
            let new_path = Self::storage_path(
                Some(&agent_owned),
                Some(target_ns),
                &new_category,
                &new_key_owned,
            );
            let existing_new = match Self::resolve_existing_at_path(
                &store,
                &new_path,
                &new_key_owned,
                &agent_owned,
                false,
            )? {
                Some(row) => Some(row),
                None => Self::rows_matching_key(&store, &new_key_owned, Some(&agent_owned))?
                    .into_iter()
                    .find(|entry| {
                        Self::metadata_str(&entry.metadata, METADATA_ZC_KEY).as_deref()
                            == Some(new_key_owned.as_str())
                            && Self::metadata_str(&entry.metadata, METADATA_ZC_AGENT).as_deref()
                                == Some(agent_owned.as_str())
                            && Self::conversation_session(&entry.metadata).is_none()
                    }),
            };

            if let Some(ref existing) = existing_new {
                ensure!(
                    existing.id != old_row.id,
                    "refused: replacement row id matches superseded row id '{}'",
                    old_row.id
                );
                ensure!(
                    Self::metadata_str(&existing.metadata, METADATA_ZC_NAMESPACE).as_deref()
                        != Some(crate::SOUL_NAMESPACE),
                    "refused: cannot overwrite a row in the reserved Soul namespace"
                );
            }

            let new_id = existing_new
                .as_ref()
                .map(|e| e.id.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let new_keywords = existing_new
                .as_ref()
                .map(|e| e.keywords.clone())
                .unwrap_or_default();
            let new_access_count = existing_new.as_ref().map(|e| e.access_count).unwrap_or(0);
            let new_last_access = existing_new.as_ref().and_then(|e| e.last_access.clone());
            let new_recall_count = existing_new.as_ref().map(|e| e.recall_count).unwrap_or(0);
            let new_query_diversity = existing_new
                .as_ref()
                .map(|e| e.query_diversity)
                .unwrap_or(0);
            let new_tier = existing_new
                .as_ref()
                .map(|e| e.tier.clone())
                .unwrap_or_else(portable_kernel::types::default_tier);
            let new_revision = existing_new.as_ref().map(|e| e.revision).unwrap_or(1);
            let new_retention_policy = existing_new
                .as_ref()
                .and_then(|e| e.retention_policy.clone());
            let new_pinned = existing_new
                .as_ref()
                .and_then(|e| e.metadata.get(METADATA_PINNED).and_then(|v| v.as_bool()))
                .unwrap_or(false);

            let new_options = StoreOptions::default().pinned(new_pinned);
            let mut new_metadata = Self::build_metadata(
                &new_key_owned,
                &new_category,
                None,
                Some(target_ns),
                Some(&agent_owned),
                &new_options,
            );
            let new_created_nanos = existing_new
                .as_ref()
                .and_then(|e| {
                    Self::metadata_str(&e.metadata, METADATA_ZC_CREATED_AT)
                        .filter(|s| !s.is_empty())
                        .or_else(|| Some(Self::canonical_sort_nanos(&e.timestamp)))
                })
                .unwrap_or_else(|| now_nanos.clone());
            if let Some(obj) = new_metadata.as_object_mut() {
                obj.insert(
                    METADATA_ZC_CREATED_AT.to_string(),
                    serde_json::Value::String(new_created_nanos),
                );
                obj.insert(
                    METADATA_ZC_UPDATED_AT.to_string(),
                    serde_json::Value::String(now_nanos.clone()),
                );
            }

            let new_entry = portable_kernel::MemoryEntry {
                id: new_id.clone(),
                path: new_path,
                summary: new_content_owned.chars().take(100).collect(),
                text: new_content_owned,
                importance: CORE_FACT_IMPORTANCE,
                timestamp: now_millis.clone(),
                valid_from: now_millis.clone(),
                valid_until: None,
                category: Self::category_to_memcore(&new_category),
                topic: String::new(),
                keywords: new_keywords,
                persons: Vec::new(),
                entities: Vec::new(),
                location: String::new(),
                source: "reiclaw".into(),
                scope: Self::scope_for_namespace(Some(target_ns)),
                archived: false,
                access_count: new_access_count,
                scored_count: 0,
                last_use_at: None,
                last_access: new_last_access,
                revision: new_revision,
                vector: embedding,
                retention_policy: new_retention_policy,
                domain: None,
                metadata: new_metadata,
                recall_count: new_recall_count,
                query_diversity: new_query_diversity,
                tier: new_tier,
            };

            if !old_row.metadata.is_object() {
                old_row.metadata = serde_json::Value::Object(serde_json::Map::new());
            }
            if let Some(obj) = old_row.metadata.as_object_mut() {
                obj.insert(
                    METADATA_SUPERSEDED_BY.to_string(),
                    serde_json::Value::String(new_id.clone()),
                );
                obj.insert(
                    METADATA_ZC_UPDATED_AT.to_string(),
                    serde_json::Value::String(now_nanos),
                );
            }
            old_row.timestamp = now_millis;

            store
                .upsert_batch(&[new_entry, old_row])
                .map_err(|e| anyhow::Error::msg(format!("tachi supersede_fact failed: {e}")))?;
            Ok(new_id)
        })
        .await?
    }

    /// Publish or replace the agent-scoped [`PinnedDigest`] (enforcing `<= 8_000` UTF-8 bytes).
    pub async fn publish_pinned_digest(&self, digest: &PinnedDigest) -> anyhow::Result<String> {
        digest.validate()?;
        let updated_ts = reiclaw_api::time::parse_rfc3339(&digest.updated_at)?;
        let ts_millis = Self::format_utc_millis(updated_ts);
        let ts_nanos = Self::format_utc_nanos(updated_ts);
        let payload = encode_sleep_json(digest)?;
        let category = Self::pinned_digest_category();
        let path = Self::storage_path(
            Some(&digest.agent_id),
            Some(SLEEP_PINNED_DIGEST_NAMESPACE),
            &category,
            SLEEP_PINNED_DIGEST_KEY,
        );
        let summary: String = digest.rendered_index.chars().take(200).collect();

        let store = self.store_handle().clone();
        let agent_id = digest.agent_id.clone();

        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let mut store = store.lock();
            let existing = Self::resolve_existing_at_path(
                &store,
                &path,
                SLEEP_PINNED_DIGEST_KEY,
                &agent_id,
                false,
            )?;
            let entry = Self::build_sleep_entry(
                existing.as_ref(),
                path,
                SLEEP_PINNED_DIGEST_KEY,
                &category,
                SLEEP_PINNED_DIGEST_NAMESPACE,
                &agent_id,
                None,
                summary,
                payload,
                Vec::new(),
                vec!["pinned_digest".into()],
                ts_millis,
                ts_nanos,
                true,
                Some("pinned"),
            );
            let id = entry.id.clone();
            store.upsert(&entry).map_err(|e| {
                anyhow::Error::msg(format!("tachi publish_pinned_digest failed: {e}"))
            })?;
            Ok(id)
        })
        .await?
    }

    /// Load the active [`PinnedDigest`] for `agent_id`, if present.
    pub async fn load_pinned_digest(&self, agent_id: &str) -> anyhow::Result<Option<PinnedDigest>> {
        validate_agent_id(agent_id)?;
        let category = Self::pinned_digest_category();
        let prefix = Self::sleep_subtree_prefix(agent_id, SLEEP_PINNED_DIGEST_NAMESPACE, &category);
        let store = self.store_handle().clone();
        let agent_owned = agent_id.to_string();

        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<PinnedDigest>> {
            let store = store.lock();
            let query = Self::sleep_recent_query(
                prefix,
                &agent_owned,
                SLEEP_PINNED_DIGEST_NAMESPACE,
                SLEEP_PINNED_DIGEST_CATEGORY,
                None,
                8,
            );
            let rows = store
                .list_recent_matching(&query)
                .map_err(|e| anyhow::Error::msg(format!("tachi load_pinned_digest failed: {e}")))?;
            for row in rows {
                if Self::is_soft_superseded(&row) {
                    continue;
                }
                if Self::metadata_str(&row.metadata, METADATA_ZC_KEY).as_deref()
                    != Some(SLEEP_PINNED_DIGEST_KEY)
                {
                    continue;
                }
                let digest: PinnedDigest = serde_json::from_str(&row.text)
                    .with_context(|| format!("failed to parse PinnedDigest '{}'", row.id))?;
                if digest.agent_id != agent_owned {
                    continue;
                }
                digest.validate()?;
                return Ok(Some(digest));
            }
            Ok(None)
        })
        .await?
    }

    /// Publish or update an agent-scoped [`DailyDigest`].
    pub async fn publish_daily_digest(&self, digest: &DailyDigest) -> anyhow::Result<String> {
        digest.validate()?;
        let created_ts = reiclaw_api::time::parse_rfc3339(&digest.created_at)?;
        let ts_millis = Self::format_utc_millis(created_ts);
        let ts_nanos = Self::format_utc_nanos(created_ts);
        let payload = encode_sleep_json(digest)?;
        let category = Self::daily_digest_category();
        let path = Self::storage_path(
            Some(&digest.agent_id),
            Some(SLEEP_DAILY_DIGEST_NAMESPACE),
            &category,
            &digest.day_key,
        );
        let summary: String = digest.narrative.chars().take(200).collect();
        let mut keywords = digest.entities.clone();
        keywords.push(digest.day_key.clone());

        let store = self.store_handle().clone();
        let agent_id = digest.agent_id.clone();
        let day_key = digest.day_key.clone();
        let entities = digest.entities.clone();

        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let mut store = store.lock();
            let existing =
                Self::resolve_existing_at_path(&store, &path, &day_key, &agent_id, false)?;
            let entry = Self::build_sleep_entry(
                existing.as_ref(),
                path,
                &day_key,
                &category,
                SLEEP_DAILY_DIGEST_NAMESPACE,
                &agent_id,
                None,
                summary,
                payload,
                entities,
                keywords,
                ts_millis,
                ts_nanos,
                false,
                Some("durable"),
            );
            let id = entry.id.clone();
            store.upsert(&entry).map_err(|e| {
                anyhow::Error::msg(format!("tachi publish_daily_digest failed: {e}"))
            })?;
            Ok(id)
        })
        .await?
    }

    /// Load agent-scoped [`DailyDigest`]s, ordered newest first, bounded by `limit`.
    pub async fn load_daily_digests(
        &self,
        agent_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DailyDigest>> {
        validate_agent_id(agent_id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let category = Self::daily_digest_category();
        let prefix = Self::sleep_subtree_prefix(agent_id, SLEEP_DAILY_DIGEST_NAMESPACE, &category);
        let store = self.store_handle().clone();
        let max_grow = self.max_list_grow.max(1);
        let agent_owned = agent_id.to_string();

        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DailyDigest>> {
            let store = store.lock();
            let mut fetch_limit = limit.max(16).min(max_grow);
            loop {
                let query = Self::sleep_recent_query(
                    prefix.clone(),
                    &agent_owned,
                    SLEEP_DAILY_DIGEST_NAMESPACE,
                    SLEEP_DAILY_DIGEST_CATEGORY,
                    None,
                    fetch_limit,
                );
                let rows = store.list_recent_matching(&query).map_err(|e| {
                    anyhow::Error::msg(format!("tachi load_daily_digests failed: {e}"))
                })?;
                let raw_len = rows.len();

                let mut digests = Vec::with_capacity(limit.min(raw_len));
                for row in rows {
                    if Self::is_soft_superseded(&row) {
                        continue;
                    }
                    let digest: DailyDigest = serde_json::from_str(&row.text)
                        .with_context(|| format!("failed to parse DailyDigest '{}'", row.id))?;
                    if digest.agent_id != agent_owned {
                        continue;
                    }
                    digests.push(digest);
                }

                if digests.len() >= limit || raw_len < fetch_limit || fetch_limit >= max_grow {
                    digests.sort_by(|a, b| {
                        let b_ts = reiclaw_api::time::parse_rfc3339(&b.created_at).ok();
                        let a_ts = reiclaw_api::time::parse_rfc3339(&a.created_at).ok();
                        b_ts.cmp(&a_ts)
                            .then_with(|| b.window_end.cmp(&a.window_end))
                            .then_with(|| b.day_key.cmp(&a.day_key))
                    });
                    digests.truncate(limit);
                    return Ok(digests);
                }
                fetch_limit = fetch_limit.saturating_mul(2).min(max_grow);
            }
        })
        .await?
    }
}

#[cfg(all(test, feature = "memorycore"))]
mod tests {
    use super::*;
    use crate::sleep_records::{EpisodeSourceKind, PINNED_DIGEST_MAX_BYTES};
    use crate::traits::Memory;
    use tempfile::TempDir;

    fn temp_memory() -> (TempDir, MemoryCore) {
        let tmp = TempDir::new().unwrap();
        let memory = MemoryCore::new("memorycore", tmp.path()).unwrap();
        (tmp, memory)
    }

    fn sample_episode(
        episode_id: &str,
        agent_id: &str,
        session_key: &str,
        started_at: &str,
        ended_at: &str,
        summary: &str,
    ) -> EpisodeRecord {
        EpisodeRecord {
            episode_id: episode_id.to_string(),
            agent_id: agent_id.to_string(),
            session_key: session_key.to_string(),
            started_at: started_at.to_string(),
            ended_at: ended_at.to_string(),
            up_to_message_id: Some(42),
            summary: summary.to_string(),
            why_and_tradeoffs: vec!["kept append-only logs for auditability".to_string()],
            settled_decisions: vec!["use single SQLite handle".to_string()],
            rejected_options: vec!["second connection pool".to_string()],
            entities: vec!["zeroclaw_project".to_string()],
            source_kind: EpisodeSourceKind::Dialogue,
        }
    }

    #[tokio::test]
    async fn delegation_episode_retry_preserves_existing_and_archived_identity() {
        let (_tmp, mem) = temp_memory();
        let mut episode = sample_episode(
            "delegation:req",
            "butler",
            "req",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "Original report",
        );
        episode.source_kind = EpisodeSourceKind::DelegationDigest;
        let id = mem.commit_episode(&episode).await.unwrap();
        episode.summary = "Replacement must not overwrite".into();
        assert_eq!(mem.commit_episode(&episode).await.unwrap(), id);
        {
            let store = mem.store_handle().lock();
            let existing = store.get_with_options(&id, true).unwrap().unwrap();
            assert!(existing.text.contains("Original report"));
            store.archive_memory(&id).unwrap();
        }
        assert_eq!(mem.commit_episode(&episode).await.unwrap(), id);
        {
            let store = mem.store_handle().lock();
            let existing = store.get_with_options(&id, true).unwrap().unwrap();
            assert!(existing.archived);
            assert!(existing.text.contains("Original report"));
        }
        episode.session_key = "foreign-session".into();
        assert!(mem.commit_episode(&episode).await.is_err());
        let store = mem.store_handle().lock();
        let existing = store.get_with_options(&id, true).unwrap().unwrap();
        assert!(existing.archived);
        assert!(existing.text.contains("Original report"));
    }

    #[tokio::test]
    async fn episode_commit_and_day_window_session_filtering_roundtrip() {
        let (_tmp, mem) = temp_memory();

        let ep1 = sample_episode(
            "ep-1",
            "zeroclaw_agent",
            "session_1",
            "2026-11-17T08:00:00Z",
            "2026-11-17T09:00:00Z",
            "Day 1 architecture alignment for zeroclaw_project",
        );
        let ep2 = sample_episode(
            "ep-2",
            "zeroclaw_agent",
            "session_2",
            "2026-11-18T01:00:00Z",
            "2026-11-18T02:00:00Z",
            "Day 2 checkpoint compaction tradeoffs",
        );
        let ep3 = EpisodeRecord {
            source_kind: EpisodeSourceKind::IdleFlush,
            ..sample_episode(
                "ep-3",
                "zeroclaw_agent",
                "session_1",
                "2026-11-18T10:00:00+08:00",
                "2026-11-18T11:30:00+08:00",
                "Day 2 session_1 idle flush with offset timestamp",
            )
        };
        let foreign = sample_episode(
            "ep-foreign",
            "other_agent",
            "session_1",
            "2026-11-18T01:30:00Z",
            "2026-11-18T02:30:00Z",
            "Foreign agent episode",
        );

        let id1 = mem.commit_episode(&ep1).await.unwrap();
        let id1_recommit = mem.commit_episode(&ep1).await.unwrap();
        assert_eq!(
            id1, id1_recommit,
            "re-committing same episode_id updates in place"
        );
        mem.commit_episode(&ep2).await.unwrap();
        mem.commit_episode(&ep3).await.unwrap();
        mem.commit_episode(&foreign).await.unwrap();

        // Day window covering ep2 (02:00Z) and ep3 (11:30+08:00 == 03:30Z), excluding ep1.
        let day2 = mem
            .read_episodes_in_window(
                "zeroclaw_agent",
                Some("2026-11-18T00:00:00Z"),
                Some("2026-11-18T23:59:59Z"),
                None,
                10,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = day2.iter().map(|e| e.episode_id.as_str()).collect();
        assert_eq!(ids, vec!["ep-3", "ep-2"]);

        // Session scoping returns only session_1 for zeroclaw_agent across all days.
        let s1 = mem
            .read_episodes_in_window("zeroclaw_agent", None, None, Some("session_1"), 10)
            .await
            .unwrap();
        let s1_ids: Vec<&str> = s1.iter().map(|e| e.episode_id.as_str()).collect();
        assert_eq!(s1_ids, vec!["ep-3", "ep-1"]);

        // Soft-superseded episodes are excluded by read_episodes_in_window.
        mem.supersede(std::slice::from_ref(&id1), "replacement-id")
            .await
            .unwrap();
        let s1_after = mem
            .read_episodes_in_window("zeroclaw_agent", None, None, Some("session_1"), 10)
            .await
            .unwrap();
        let s1_after_ids: Vec<&str> = s1_after.iter().map(|e| e.episode_id.as_str()).collect();
        assert_eq!(s1_after_ids, vec!["ep-3"]);

        // Soul recall never collides with episode storage.
        let soul_rows = mem
            .recall_namespaced(crate::SOUL_NAMESPACE, "*", 10, None, None, None)
            .await
            .unwrap();
        assert!(soul_rows.is_empty());
    }

    #[tokio::test]
    async fn pinned_digest_enforces_byte_limit_and_agent_scoping() {
        let (_tmp, mem) = temp_memory();

        let exact_limit = "a".repeat(PINNED_DIGEST_MAX_BYTES);
        let ok_digest = PinnedDigest {
            agent_id: "agent_a".to_string(),
            rendered_index: exact_limit.clone(),
            updated_at: "2026-11-18T04:00:00Z".to_string(),
            source_day_key: Some("2026-11-18".to_string()),
        };
        mem.publish_pinned_digest(&ok_digest).await.unwrap();

        let loaded = mem.load_pinned_digest("agent_a").await.unwrap().unwrap();
        assert_eq!(loaded, ok_digest);
        assert!(mem.load_pinned_digest("agent_b").await.unwrap().is_none());

        // Over 8_000 UTF-8 bytes (both ASCII and multi-byte UTF-8) must be rejected.
        let over_ascii = PinnedDigest {
            agent_id: "agent_a".to_string(),
            rendered_index: "a".repeat(PINNED_DIGEST_MAX_BYTES + 1),
            updated_at: "2026-11-18T04:05:00Z".to_string(),
            source_day_key: None,
        };
        let err = mem.publish_pinned_digest(&over_ascii).await.unwrap_err();
        assert!(err.to_string().contains("8000"));

        // 2,667 3-byte UTF-8 chars = 8,001 bytes > 8,000 bytes.
        let over_utf8 = PinnedDigest {
            agent_id: "agent_a".to_string(),
            rendered_index: "\u{4e2d}".repeat(2_667),
            updated_at: "2026-11-18T04:05:00Z".to_string(),
            source_day_key: None,
        };
        assert!(mem.publish_pinned_digest(&over_utf8).await.is_err());

        // Previously stored valid digest remains intact after rejected write.
        let still_valid = mem.load_pinned_digest("agent_a").await.unwrap().unwrap();
        assert_eq!(still_valid.rendered_index.len(), PINNED_DIGEST_MAX_BYTES);
    }

    #[tokio::test]
    async fn soul_namespace_and_prefix_are_rejected_across_all_sleep_paths() {
        let (_tmp, mem) = temp_memory();

        let soul_agent_ep = sample_episode(
            "ep-soul",
            crate::SOUL_NAMESPACE,
            "session_1",
            "2026-11-18T01:00:00Z",
            "2026-11-18T02:00:00Z",
            "should fail",
        );
        assert!(mem.commit_episode(&soul_agent_ep).await.is_err());

        let soul_key_ep = sample_episode(
            "soul::ep-1",
            "agent_a",
            "session_1",
            "2026-11-18T01:00:00Z",
            "2026-11-18T02:00:00Z",
            "should fail",
        );
        assert!(mem.commit_episode(&soul_key_ep).await.is_err());

        assert!(
            mem.read_episodes_in_window(crate::SOUL_NAMESPACE, None, None, None, 10)
                .await
                .is_err()
        );
        assert!(
            mem.read_episodes_in_window("agent_a", None, None, Some("soul::s1"), 10)
                .await
                .is_err()
        );

        let soul_pinned = PinnedDigest {
            agent_id: "soul::agent".to_string(),
            rendered_index: "index".to_string(),
            updated_at: "2026-11-18T04:00:00Z".to_string(),
            source_day_key: None,
        };
        assert!(mem.publish_pinned_digest(&soul_pinned).await.is_err());
        assert!(mem.load_pinned_digest(crate::SOUL_NAMESPACE).await.is_err());

        let soul_daily = DailyDigest {
            day_key: "soul::2026-11-18".to_string(),
            agent_id: "agent_a".to_string(),
            window_start: "2026-11-17T04:00:00Z".to_string(),
            window_end: "2026-11-18T04:00:00Z".to_string(),
            narrative: "should fail".to_string(),
            settled_decisions: vec![],
            why_and_tradeoffs: vec![],
            latent_insights: vec![],
            action_candidates: vec![],
            entities: vec![],
            created_at: "2026-11-18T04:00:00Z".to_string(),
            owner_edited: false,
            action_decisions: std::collections::BTreeMap::new(),
        };
        assert!(mem.publish_daily_digest(&soul_daily).await.is_err());
        assert!(
            mem.load_daily_digests(crate::SOUL_NAMESPACE, 5)
                .await
                .is_err()
        );

        // Seed a valid Soul entry and verify supersede_fact refuses both Soul namespace and soul:: keys.
        mem.store_with_options_and_agent(
            "soul::agent_a::disposition",
            "protected soul line",
            MemoryCategory::Core,
            None,
            StoreOptions {
                namespace: Some(crate::SOUL_NAMESPACE.into()),
                ..StoreOptions::default()
            },
            Some("agent_a"),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "ordinary_fact",
            "ordinary content",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("agent_a"),
        )
        .await
        .unwrap();

        assert!(
            mem.supersede_fact(
                "agent_a",
                "soul::agent_a::disposition",
                "new_fact",
                "override",
                None
            )
            .await
            .is_err()
        );
        assert!(
            mem.supersede_fact(
                "agent_a",
                "ordinary_fact",
                "soul::new_fact",
                "override",
                None
            )
            .await
            .is_err()
        );
        assert!(
            mem.supersede_fact(
                "agent_a",
                "ordinary_fact",
                "new_fact",
                "override",
                Some(crate::SOUL_NAMESPACE)
            )
            .await
            .is_err()
        );
        assert!(
            mem.supersede_fact(
                crate::SOUL_NAMESPACE,
                "ordinary_fact",
                "new_fact",
                "override",
                None
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn supersede_fact_enforces_agent_scope_and_hides_loser_from_recall() {
        let (_tmp, mem) = temp_memory();

        mem.store_with_agent(
            "office_city",
            "zeroclaw_project office is in city_alpha",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("agent_a"),
        )
        .await
        .unwrap();

        // Cross-agent supersession must fail and leave agent_a's fact untouched.
        let cross_err = mem
            .supersede_fact(
                "agent_b",
                "office_city",
                "office_city_v2",
                "zeroclaw_project office moved to city_beta",
                None,
            )
            .await
            .unwrap_err();
        assert!(cross_err.to_string().contains("not found"));

        let still_active = mem
            .get_for_agent("office_city", "agent_a")
            .await
            .unwrap()
            .unwrap();
        assert!(still_active.superseded_by.is_none());

        // Same-agent supersession succeeds, writes Core replacement, and stamps superseded_by.
        let new_id = mem
            .supersede_fact(
                "agent_a",
                "office_city",
                "office_city_v2",
                "zeroclaw_project office moved to city_beta",
                None,
            )
            .await
            .unwrap();

        let old_row = mem
            .get_for_agent("office_city", "agent_a")
            .await
            .unwrap()
            .expect("superseded row remains inspectable for audit lineage");
        assert_eq!(old_row.superseded_by.as_deref(), Some(new_id.as_str()));

        let new_row = mem
            .get_for_agent("office_city_v2", "agent_a")
            .await
            .unwrap()
            .expect("replacement row present");
        assert_eq!(new_row.id, new_id);
        assert_eq!(new_row.category, MemoryCategory::Core);
        assert_eq!(
            new_row.content,
            "zeroclaw_project office moved to city_beta"
        );
        assert!(new_row.superseded_by.is_none());

        // Recall (keyword, "*", and "") omits old_key and surfaces new_key.
        let recalled = mem
            .recall_for_agents(
                &["agent_a"],
                "zeroclaw_project office",
                10,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(recalled.iter().all(|e| e.key != "office_city"));
        assert!(recalled.iter().any(|e| e.key == "office_city_v2"));

        for recent_query in ["*", ""] {
            let recent = mem
                .recall_for_agents(&["agent_a"], recent_query, 10, None, None, None)
                .await
                .unwrap();
            assert!(recent.iter().all(|e| e.key != "office_city"));
            assert!(recent.iter().any(|e| e.key == "office_city_v2"));
        }

        // Superseding with an explicit non-soul namespace sets the replacement's namespace.
        let ns_id = mem
            .supersede_fact(
                "agent_a",
                "office_city_v2",
                "office_city_v3",
                "zeroclaw_project office moved to city_gamma",
                Some("project_facts"),
            )
            .await
            .unwrap();
        let v3_row = mem
            .get_for_agent("office_city_v3", "agent_a")
            .await
            .unwrap()
            .expect("v3 row present");
        assert_eq!(v3_row.id, ns_id);
        assert_eq!(v3_row.namespace, "project_facts");
        let v2_row = mem
            .get_for_agent("office_city_v2", "agent_a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v2_row.superseded_by.as_deref(), Some(ns_id.as_str()));

        // Attempting to supersede an already-superseded fact fails.
        assert!(
            mem.supersede_fact(
                "agent_a",
                "office_city",
                "office_city_v4",
                "city_delta",
                None
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn daily_digest_roundtrip_is_agent_scoped_and_newest_first() {
        let (_tmp, mem) = temp_memory();

        let d1 = DailyDigest {
            day_key: "2026-11-17".to_string(),
            agent_id: "agent_a".to_string(),
            window_start: "2026-11-16T04:00:00Z".to_string(),
            window_end: "2026-11-17T04:00:00Z".to_string(),
            narrative: "Day 1 summary for zeroclaw_project".to_string(),
            settled_decisions: vec!["adopt ADR-025".to_string()],
            why_and_tradeoffs: vec!["avoided per-turn LLM calls".to_string()],
            latent_insights: vec!["short follow-ups benefit from pinned index".to_string()],
            action_candidates: vec!["review benchmark metrics".to_string()],
            entities: vec!["zeroclaw_project".to_string()],
            created_at: "2026-11-17T04:01:00Z".to_string(),
            owner_edited: false,
            action_decisions: std::collections::BTreeMap::new(),
        };
        let d2 = DailyDigest {
            day_key: "2026-11-18".to_string(),
            agent_id: "agent_a".to_string(),
            window_start: "2026-11-17T04:00:00Z".to_string(),
            window_end: "2026-11-18T04:00:00Z".to_string(),
            narrative: "Day 2 summary with <think>escaped</think> check".to_string(),
            settled_decisions: vec!["landed sleep records".to_string()],
            why_and_tradeoffs: vec![],
            latent_insights: vec![],
            action_candidates: vec![],
            entities: vec!["reiclaw_runtime".to_string()],
            created_at: "2026-11-18T04:01:00Z".to_string(),
            owner_edited: false,
            action_decisions: std::collections::BTreeMap::new(),
        };
        let other = DailyDigest {
            day_key: "2026-11-18".to_string(),
            agent_id: "agent_b".to_string(),
            window_start: "2026-11-17T04:00:00Z".to_string(),
            window_end: "2026-11-18T04:00:00Z".to_string(),
            narrative: "Other agent digest".to_string(),
            settled_decisions: vec![],
            why_and_tradeoffs: vec![],
            latent_insights: vec![],
            action_candidates: vec![],
            entities: vec![],
            created_at: "2026-11-18T04:02:00Z".to_string(),
            owner_edited: false,
            action_decisions: std::collections::BTreeMap::new(),
        };

        // Publish out of chronological order to verify newest-first ordering.
        mem.publish_daily_digest(&d2).await.unwrap();
        mem.publish_daily_digest(&d1).await.unwrap();
        mem.publish_daily_digest(&other).await.unwrap();

        let loaded = mem.load_daily_digests("agent_a", 10).await.unwrap();
        assert_eq!(loaded, vec![d2.clone(), d1]);

        let limited = mem.load_daily_digests("agent_a", 1).await.unwrap();
        assert_eq!(limited, vec![d2]);

        let other_loaded = mem.load_daily_digests("agent_b", 10).await.unwrap();
        assert_eq!(other_loaded, vec![other]);
    }
}
