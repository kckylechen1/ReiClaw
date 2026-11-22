//! Sleep-memory primitives over `MemoryCore`'s single `MemoryStore` handle
//! (ADR-025 Wave 1 Slice 2/3B).

use super::sleep_typed::{self, SleepKind};
use super::{
    DEFAULT_NAMESPACE, METADATA_PINNED, METADATA_SESSION_ID, METADATA_SUPERSEDED_BY,
    METADATA_ZC_AGENT, METADATA_ZC_CATEGORY, METADATA_ZC_CREATED_AT, METADATA_ZC_KEY,
    METADATA_ZC_NAMESPACE, METADATA_ZC_UPDATED_AT, MemoryCore,
};
use crate::point_scope::PointScope;
use crate::sleep_records::{
    DailyDigest, EpisodeRecord, EpisodeSourceKind, PinnedDigest, validate_agent_id,
    validate_non_soul_key, validate_non_soul_namespace,
};
use crate::traits::{MemoryCategory, StoreOptions};
use anyhow::{Context, bail, ensure};
use portable_kernel::MemoryStore;
use portable_kernel::store::scoped_read::{MetadataTextFilter, RecentMemoryQuery, TextMembership};
use serde_json::Value;
use std::collections::HashSet;
use uuid::Uuid;

const SLEEP_EPISODE_NAMESPACE: &str = "sleep_episode";
const SLEEP_EPISODE_CATEGORY: &str = "sleep_episode";
const SLEEP_PINNED_DIGEST_NAMESPACE: &str = "sleep_pinned_digest";
const SLEEP_PINNED_DIGEST_CATEGORY: &str = "sleep_pinned_digest";
const SLEEP_PINNED_DIGEST_KEY: &str = "sleep:pinned_digest";
const SLEEP_DAILY_DIGEST_NAMESPACE: &str = "sleep_daily_digest";
const SLEEP_DAILY_DIGEST_CATEGORY: &str = "sleep_daily_digest";
const CORE_FACT_IMPORTANCE: f64 = 0.7;

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

    pub(super) fn legacy_sleep_kind(row: &portable_kernel::MemoryEntry) -> Option<SleepKind> {
        let namespace = row.metadata[METADATA_ZC_NAMESPACE].as_str()?;
        if row.metadata[METADATA_ZC_CATEGORY] != namespace {
            return None;
        }
        match namespace {
            SLEEP_EPISODE_NAMESPACE => Some(SleepKind::Episode),
            SLEEP_PINNED_DIGEST_NAMESPACE => Some(SleepKind::Pinned),
            SLEEP_DAILY_DIGEST_NAMESPACE => Some(SleepKind::Daily),
            _ => None,
        }
    }

    pub(super) fn legacy_slot_key(
        row: &portable_kernel::MemoryEntry,
    ) -> Option<(String, SleepKind, String)> {
        let kind = Self::legacy_sleep_kind(row)?;
        let agent = Self::metadata_str(&row.metadata, METADATA_ZC_AGENT)?;
        let key = row.metadata[METADATA_ZC_KEY].as_str()?;
        let slot = match kind {
            SleepKind::Pinned => (key == SLEEP_PINNED_DIGEST_KEY).then_some("pinned_digest")?,
            SleepKind::Episode | SleepKind::Daily => key,
        };
        Some((agent, kind, slot.to_owned()))
    }

    pub(super) fn legacy_sleep_slot(
        store: &MemoryStore,
        kind: SleepKind,
        agent: &str,
        slot: &str,
    ) -> anyhow::Result<Option<portable_kernel::MemoryEntry>> {
        let (namespace, category, key) = match kind {
            SleepKind::Episode => (SLEEP_EPISODE_NAMESPACE, Self::episode_category(), slot),
            SleepKind::Pinned => (
                SLEEP_PINNED_DIGEST_NAMESPACE,
                Self::pinned_digest_category(),
                SLEEP_PINNED_DIGEST_KEY,
            ),
            SleepKind::Daily => (
                SLEEP_DAILY_DIGEST_NAMESPACE,
                Self::daily_digest_category(),
                slot,
            ),
        };
        let path = Self::storage_path(Some(agent), Some(namespace), &category, key);
        let mut query = RecentMemoryQuery::new(path.clone(), 1);
        query.include_archived = true;
        for (field, value) in [
            (METADATA_ZC_AGENT, agent),
            (METADATA_ZC_KEY, key),
            (METADATA_ZC_CATEGORY, namespace),
            (METADATA_ZC_NAMESPACE, namespace),
        ] {
            query.filters.push(MetadataTextFilter {
                field: field.into(),
                values: vec![value.into()],
                membership: TextMembership::AnyOf,
                missing_value: None,
            });
        }
        Ok(store
            .list_recent_matching(&query)?
            .into_iter()
            .find(|r| r.path == path))
    }

    // Legacy rows carry the agent twice: in storage metadata and in the DTO.
    pub(super) fn rename_legacy_sleep_payload(
        store: &MemoryStore,
        row: &mut portable_kernel::MemoryEntry,
        from: &str,
        to: &str,
    ) -> anyhow::Result<()> {
        let Some(kind) = Self::legacy_sleep_kind(row) else {
            return Ok(());
        };
        let mut payload: Value = serde_json::from_str(&row.text)?;
        ensure!(
            payload["agent_id"] == from,
            "legacy sleep record agent mismatch"
        );
        payload["agent_id"] = Value::String(to.into());
        kind.validate_payload(&payload)?;
        let key = row.metadata[METADATA_ZC_KEY]
            .as_str()
            .context("missing legacy sleep key")?;
        let slot = if kind == SleepKind::Pinned {
            "pinned_digest"
        } else {
            key
        };
        ensure!(
            Self::legacy_sleep_slot(store, kind, to, slot)?.is_none(),
            "destination legacy sleep record already exists"
        );
        if kind.current(store, from, slot)?.is_none()
            && let Some(existing) = kind.current(store, to, slot)?
        {
            ensure!(
                kind.verified_payload(existing)? == payload,
                "destination sleep record already exists"
            );
        }
        row.text = serde_json::to_string(&payload)?.replace('<', "\\u003c");
        Ok(())
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

    /// Append a new revision, or recover the original delegated report on replay.
    pub async fn commit_episode(&self, episode: &EpisodeRecord) -> anyhow::Result<String> {
        episode.validate()?;
        let episode = episode.clone();
        let handle = self.store.clone();
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = handle.lock();
            let _guard = sleep_typed::write_guard(&db_path)?;
            let current =
                SleepKind::Episode.current(&store, &episode.agent_id, &episode.episode_id)?;
            let predecessor = current.as_ref().map(|r| r.id.clone());
            let existing = if let Some(record) = current {
                let id = record.id.clone();
                Some((
                    id,
                    serde_json::from_value::<EpisodeRecord>(
                        SleepKind::Episode.verified_payload(record)?,
                    )?,
                ))
            } else {
                Self::legacy_sleep_slot(
                    &store,
                    SleepKind::Episode,
                    &episode.agent_id,
                    &episode.episode_id,
                )?
                .map(|row| serde_json::from_str(&row.text).map(|stored| (row.id, stored)))
                .transpose()?
            };
            if let Some((id, stored)) = existing
                && (episode.source_kind == EpisodeSourceKind::DelegationDigest
                    || stored.source_kind == EpisodeSourceKind::DelegationDigest)
            {
                stored.validate()?;
                ensure!(
                    episode.episode_id == stored.episode_id
                        && episode.agent_id == stored.agent_id
                        && episode.session_key == stored.session_key
                        && episode.source_kind == stored.source_kind,
                    "delegation Episode identity conflicts"
                );
                return Ok(id);
            }
            SleepKind::Episode.append(
                &mut store,
                serde_json::to_value(episode)?,
                predecessor.as_deref(),
            )
        })
        .await?
    }

    // A typed successor can move out of the requested session or window.
    // Check the logical slot without those filters before admitting legacy data.
    fn legacy_sleep_rows<T>(
        store: &MemoryStore,
        kind: SleepKind,
        agent: &str,
        mut query: RecentMemoryQuery,
        limit: usize,
        max_grow: usize,
        decode: impl Fn(&str, &str) -> anyhow::Result<Option<T>>,
    ) -> anyhow::Result<Vec<T>> {
        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        let mut cutoff = None;
        query.limit = limit.max(16).min(max_grow);
        loop {
            let rows = store.list_recent_matching(&query)?;
            let full = rows.len() == query.limit;
            for row in rows {
                if cutoff.as_ref().is_some_and(|ts| &row.timestamp < ts) {
                    return Ok(selected);
                }
                let Some(key) = row.metadata[METADATA_ZC_KEY].as_str() else {
                    continue;
                };
                if seen.contains(key) || kind.current(store, agent, key)?.is_some() {
                    continue;
                }
                if let Some(value) = decode(key, &row.text)? {
                    seen.insert(key.to_string());
                    selected.push(value);
                    if selected.len() == limit {
                        cutoff = Some(row.timestamp);
                    }
                }
            }
            if !full || query.limit >= max_grow {
                return Ok(selected);
            }
            query.limit = query.limit.saturating_mul(2).min(max_grow);
        }
    }

    /// Read active episodes in an inclusive instant window, newest first.
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
        let since = since
            .map(reiclaw_api::time::parse_rfc3339)
            .transpose()
            .context("invalid since timestamp for read_episodes_in_window")?;
        let until = until
            .map(reiclaw_api::time::parse_rfc3339)
            .transpose()
            .context("invalid until timestamp for read_episodes_in_window")?;
        if limit == 0 || matches!((since,until),(Some(s),Some(u)) if s > u) {
            return Ok(Vec::new());
        }
        let agent = agent_id.to_owned();
        let session = session_id.map(str::to_owned);
        let handle = self.store.clone();
        let max_grow = self.max_list_grow;
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let store = handle.lock();
            let _guard = sleep_typed::read_guard(&db_path)?;
            let kind = SleepKind::Episode;
            let mut query = kind.query(Some(&agent));
            if let Some(sid) = &session {
                query.key_filters.insert("session_key".into(), sid.clone());
            }
            query.since = since.map(|t| t.to_string());
            query.until = until.map(|t| t.to_string());
            let mut episodes = Vec::new();
            for record in sleep_typed::ordered(&store, kind, query, limit, false)? {
                let ep: EpisodeRecord = serde_json::from_value(kind.verified_payload(record)?)?;
                episodes.push((reiclaw_api::time::parse_rfc3339(&ep.ended_at)?, ep));
            }
            let prefix = Self::sleep_subtree_prefix(
                &agent,
                SLEEP_EPISODE_NAMESPACE,
                &Self::episode_category(),
            );
            let mut legacy = Self::sleep_recent_query(
                prefix,
                &agent,
                SLEEP_EPISODE_NAMESPACE,
                SLEEP_EPISODE_CATEGORY,
                session.as_deref(),
                limit,
            );
            legacy.since = since.map(Self::format_utc_millis);
            legacy.until = until.map(ceil_utc_millis);
            episodes.extend(Self::legacy_sleep_rows(
                &store,
                kind,
                &agent,
                legacy,
                limit,
                max_grow,
                |key, text| {
                    let ep: EpisodeRecord = serde_json::from_str(text)?;
                    if ep.agent_id != agent
                        || ep.episode_id != key
                        || session.as_ref().is_some_and(|sid| sid != &ep.session_key)
                    {
                        return Ok(None);
                    }
                    ep.validate()?;
                    let time = reiclaw_api::time::parse_rfc3339(&ep.ended_at)?;
                    Ok(
                        (since.is_none_or(|s| time >= s) && until.is_none_or(|u| time <= u))
                            .then_some((time, ep)),
                    )
                },
            )?);
            episodes.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
            Ok(episodes.into_iter().take(limit).map(|(_, ep)| ep).collect())
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

    async fn publish_sleep_digest(
        &self,
        kind: SleepKind,
        agent: &str,
        slot: &str,
        payload: Value,
    ) -> anyhow::Result<String> {
        let handle = self.store.clone();
        let db_path = self.db_path.clone();
        let agent = agent.to_owned();
        let slot = slot.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut store = handle.lock();
            let _guard = sleep_typed::write_guard(&db_path)?;
            let current = kind.current(&store, &agent, &slot)?;
            kind.append(&mut store, payload, current.as_ref().map(|r| r.id.as_str()))
        })
        .await?
    }

    pub async fn publish_pinned_digest(&self, digest: &PinnedDigest) -> anyhow::Result<String> {
        digest.validate()?;
        self.publish_sleep_digest(
            SleepKind::Pinned,
            &digest.agent_id,
            "pinned_digest",
            serde_json::to_value(digest)?,
        )
        .await
    }

    /// Load the active [`PinnedDigest`] for `agent_id`, if present.
    pub async fn load_pinned_digest(&self, agent_id: &str) -> anyhow::Result<Option<PinnedDigest>> {
        validate_agent_id(agent_id)?;
        let category = Self::pinned_digest_category();
        let prefix = Self::sleep_subtree_prefix(agent_id, SLEEP_PINNED_DIGEST_NAMESPACE, &category);
        let store = self.store_handle().clone();
        let agent_owned = agent_id.to_string();

        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<PinnedDigest>> {
            let store = store.lock();
            let _guard = sleep_typed::read_guard(&db_path)?;
            if let Some(record) =
                SleepKind::Pinned.current(&store, &agent_owned, "pinned_digest")?
            {
                return Ok(Some(serde_json::from_value(
                    SleepKind::Pinned.verified_payload(record)?,
                )?));
            }
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

    pub async fn publish_daily_digest(&self, digest: &DailyDigest) -> anyhow::Result<String> {
        digest.validate()?;
        self.publish_sleep_digest(
            SleepKind::Daily,
            &digest.agent_id,
            &digest.day_key,
            serde_json::to_value(digest)?,
        )
        .await
    }

    pub async fn load_daily_digests(
        &self,
        agent_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DailyDigest>> {
        validate_agent_id(agent_id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let agent = agent_id.to_owned();
        let handle = self.store.clone();
        let max_grow = self.max_list_grow;
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let store = handle.lock();
            let _guard = sleep_typed::read_guard(&db_path)?;
            let kind = SleepKind::Daily;
            let mut digests = Vec::new();
            for record in sleep_typed::ordered(&store, kind, kind.query(Some(&agent)), limit, true)?
            {
                let digest: DailyDigest = serde_json::from_value(kind.verified_payload(record)?)?;
                digests.push((
                    reiclaw_api::time::parse_rfc3339(&digest.created_at)?,
                    reiclaw_api::time::parse_rfc3339(&digest.window_end)?,
                    digest,
                ));
            }
            let prefix = Self::sleep_subtree_prefix(
                &agent,
                SLEEP_DAILY_DIGEST_NAMESPACE,
                &Self::daily_digest_category(),
            );
            let legacy = Self::sleep_recent_query(
                prefix,
                &agent,
                SLEEP_DAILY_DIGEST_NAMESPACE,
                SLEEP_DAILY_DIGEST_CATEGORY,
                None,
                limit,
            );
            digests.extend(Self::legacy_sleep_rows(
                &store,
                kind,
                &agent,
                legacy,
                limit,
                max_grow,
                |key, text| {
                    let digest: DailyDigest = serde_json::from_str(text)?;
                    if digest.agent_id != agent || digest.day_key != key {
                        return Ok(None);
                    }
                    digest.validate()?;
                    Ok(Some((
                        reiclaw_api::time::parse_rfc3339(&digest.created_at)?,
                        reiclaw_api::time::parse_rfc3339(&digest.window_end)?,
                        digest,
                    )))
                },
            )?);
            digests.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| b.1.cmp(&a.1))
                    .then_with(|| b.2.day_key.cmp(&a.2.day_key))
            });
            Ok(digests
                .into_iter()
                .take(limit)
                .map(|(_, _, digest)| digest)
                .collect())
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

    async fn legacy_row(
        mem: &MemoryCore,
        agent: &str,
        namespace: &str,
        key: &str,
        session: Option<&str>,
        payload: &impl serde::Serialize,
        time: &str,
    ) -> String {
        let category = MemoryCategory::Custom(namespace.into());
        mem.store_with_options_and_agent(
            key,
            &serde_json::to_string(payload).unwrap(),
            category.clone(),
            session,
            StoreOptions {
                namespace: Some(namespace.into()),
                ..Default::default()
            },
            Some(agent),
        )
        .await
        .unwrap();
        let path = MemoryCore::storage_path(Some(agent), Some(namespace), &category, key);
        let mut store = mem.store.lock();
        let mut row = store.list_by_path(&path, 1, false).unwrap().pop().unwrap();
        row.timestamp =
            MemoryCore::format_utc_millis(reiclaw_api::time::parse_rfc3339(time).unwrap());
        store.upsert(&row).unwrap();
        row.id
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
        let id = legacy_row(
            &mem,
            &episode.agent_id,
            SLEEP_EPISODE_NAMESPACE,
            &episode.episode_id,
            Some(&episode.session_key),
            &episode,
            &episode.ended_at,
        )
        .await;
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
        assert_ne!(id1, id1_recommit);
        let previous = mem.store.lock().get_typed_record(&id1).unwrap().unwrap();
        assert_eq!(
            previous.superseded_by.as_deref(),
            Some(id1_recommit.as_str())
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

        // The replacement moved sessions; the superseded revision is invisible.
        let mut replacement = ep1.clone();
        replacement.session_key = "session_3".into();
        mem.commit_episode(&replacement).await.unwrap();
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
    async fn migrated_episodes_shadow_legacy_outside_the_new_window_and_session() {
        let (_tmp, mem) = temp_memory();
        let mut ep = sample_episode(
            "same",
            "agent",
            "old",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00.000000001Z",
            "legacy",
        );
        legacy_row(
            &mem,
            &ep.agent_id,
            SLEEP_EPISODE_NAMESPACE,
            &ep.episode_id,
            Some("old"),
            &ep,
            &ep.ended_at,
        )
        .await;
        assert_eq!(
            mem.read_episodes_in_window("agent", None, None, Some("old"), 1)
                .await
                .unwrap(),
            [ep.clone()]
        );
        ep.ended_at = "2026-11-18T01:00:00.000000002Z".into();
        ep.session_key = "new".into();
        mem.commit_episode(&ep).await.unwrap();
        assert!(
            mem.read_episodes_in_window("agent", None, None, Some("old"), 1)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            mem.read_episodes_in_window(
                "agent",
                None,
                Some("2026-11-18T01:00:00.000000001Z"),
                None,
                1
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            mem.read_episodes_in_window(
                "agent",
                Some(&ep.ended_at),
                Some(&ep.ended_at),
                Some("new"),
                usize::MAX
            )
            .await
            .unwrap(),
            [ep]
        );
    }

    #[tokio::test]
    async fn typed_delegation_replay_preserves_content_and_rejects_identity_changes() {
        let (_tmp, mem) = temp_memory();
        let mut ep = sample_episode(
            "delegated",
            "agent",
            "sess",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "first report",
        );
        ep.source_kind = EpisodeSourceKind::DelegationDigest;
        let id = mem.commit_episode(&ep).await.unwrap();
        ep.summary = "replay".into();
        assert_eq!(mem.commit_episode(&ep).await.unwrap(), id);
        ep.session_key = "other".into();
        assert!(mem.commit_episode(&ep).await.is_err());
        ep.session_key = "sess".into();
        ep.source_kind = EpisodeSourceKind::Dialogue;
        assert!(mem.commit_episode(&ep).await.is_err());
        let record = mem.store.lock().get_typed_record(&id).unwrap().unwrap();
        assert_eq!(record.payload["summary"], "first report");
        assert!(record.superseded_by.is_none());
    }

    #[tokio::test]
    async fn typed_pinned_lineage_counts_rename_and_scoped_erasure() {
        let (tmp, mem) = temp_memory();
        let mut pin = PinnedDigest {
            agent_id: "from".into(),
            rendered_index: "v1".into(),
            updated_at: "2026-11-18T01:00:00Z".into(),
            source_day_key: None,
        };
        let mut foreign = pin.clone();
        foreign.agent_id = "foreign".into();
        let other_id = mem.publish_pinned_digest(&foreign).await.unwrap();
        let first = mem.publish_pinned_digest(&pin).await.unwrap();
        pin.rendered_index = "v2".into();
        let second = mem.publish_pinned_digest(&pin).await.unwrap();
        {
            let store = mem.store.lock();
            let v1 = store.get_typed_record(&first).unwrap().unwrap();
            let v2 = store.get_typed_record(&second).unwrap().unwrap();
            assert_eq!(v1.superseded_by.as_deref(), Some(second.as_str()));
            assert_eq!((v1.revision, v2.revision), (1, 2));
            assert_eq!(store.stats(false).unwrap().total, 0);
        }
        assert_eq!(
            mem.load_pinned_digest("from").await.unwrap(),
            Some(pin.clone())
        );
        assert_eq!(mem.count().await.unwrap(), 2);
        let stats = mem.stats().await.unwrap();
        assert_eq!((stats.total_rows, stats.pinned_rows), (2, 2));
        assert_eq!(stats.by_category, vec![("other".into(), 2)]);
        assert_eq!(mem.count_agent("from").await.unwrap(), 1);
        assert!(mem.rename_agent("from", "foreign").await.is_err());
        assert_eq!(mem.rename_agent("from", "to").await.unwrap(), 1);
        assert_eq!(mem.count_agent("from").await.unwrap(), 0);
        pin.agent_id = "to".into();
        assert_eq!(
            mem.load_pinned_digest("to").await.unwrap(),
            Some(pin.clone())
        );
        mem.publish_pinned_digest(&pin).await.unwrap();
        assert_eq!(mem.purge_agent("to").await.unwrap(), 2);
        assert_eq!(mem.count().await.unwrap(), 1);
        let store = mem.store.lock();
        assert!(store.get_typed_record(&first).unwrap().is_none());
        assert!(store.get_typed_record(&second).unwrap().is_none());
        assert!(store.get_typed_record(&other_id).unwrap().is_some());
        let conn = rusqlite::Connection::open_with_flags(
            tmp.path().join("memory/memorycore.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM typed_record_supersessions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn typed_window_and_rename_cover_a_time_bucket_larger_than_one_kernel_page() {
        let (_tmp, mem) = temp_memory();
        let mut ep = sample_episode(
            "0",
            "old",
            "session",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "tied",
        );
        {
            let mut store = mem.store.lock();
            for i in 0..1001 {
                ep.episode_id = format!("ep-{i}");
                SleepKind::Episode
                    .append(&mut store, serde_json::to_value(&ep).unwrap(), None)
                    .unwrap();
            }
        }
        let rows = mem
            .read_episodes_in_window(
                "old",
                Some(&ep.ended_at),
                Some(&ep.ended_at),
                Some("session"),
                1001,
            )
            .await
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| &r.episode_id)
                .collect::<HashSet<_>>()
                .len(),
            1001
        );
        assert_eq!(mem.rename_agent("old", "new").await.unwrap(), 1001);
        assert_eq!(mem.count_agent("old").await.unwrap(), 0);
        assert_eq!(mem.count_agent("new").await.unwrap(), 1001);
        let rows = mem
            .read_episodes_in_window("new", None, None, None, 1)
            .await
            .unwrap();
        assert_eq!(rows[0].agent_id, "new");
    }

    #[tokio::test]
    async fn separately_opened_handles_extend_one_pinned_lineage() {
        let (tmp, first) = temp_memory();
        let second = MemoryCore::new("other", tmp.path()).unwrap();
        let pin = PinnedDigest {
            agent_id: "agent".into(),
            rendered_index: "first".into(),
            updated_at: "2026-11-18T01:00:00Z".into(),
            source_day_key: None,
        };
        let other = PinnedDigest {
            rendered_index: "other".into(),
            ..pin.clone()
        };
        let (a, b) = tokio::join!(
            first.publish_pinned_digest(&pin),
            second.publish_pinned_digest(&other)
        );
        assert_ne!(a.unwrap(), b.unwrap());
        let current = SleepKind::Pinned
            .current(&first.store.lock(), "agent", "pinned_digest")
            .unwrap()
            .unwrap();
        assert_eq!(current.revision, 2);
    }

    #[tokio::test]
    async fn namespace_purge_erases_only_that_kind_across_agents() {
        let (_tmp, mem) = temp_memory();
        let pin = PinnedDigest {
            agent_id: "first".into(),
            rendered_index: "v1".into(),
            updated_at: "2026-11-18T01:00:00Z".into(),
            source_day_key: None,
        };
        mem.publish_pinned_digest(&pin).await.unwrap();
        mem.publish_pinned_digest(&pin).await.unwrap();
        let other = PinnedDigest {
            agent_id: "second".into(),
            ..pin
        };
        mem.publish_pinned_digest(&other).await.unwrap();
        let ep = sample_episode(
            "ep",
            "first",
            "session",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "untouched",
        );
        mem.commit_episode(&ep).await.unwrap();
        assert_eq!(
            mem.purge_namespace(SLEEP_PINNED_DIGEST_NAMESPACE)
                .await
                .unwrap(),
            3
        );
        assert!(mem.load_pinned_digest("first").await.unwrap().is_none());
        assert!(mem.load_pinned_digest("second").await.unwrap().is_none());
        assert_eq!(
            mem.read_episodes_in_window("first", None, None, None, 1)
                .await
                .unwrap(),
            [ep]
        );
    }

    #[tokio::test]
    async fn scoped_count_includes_visible_typed_slots_and_excludes_foreign_agents() {
        use crate::agent_scoped::AgentScopedMemory;
        let (_tmp, mem) = temp_memory();
        let ep = sample_episode(
            "ep",
            "alpha",
            "old",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "visible",
        );
        legacy_row(
            &mem,
            "alpha",
            SLEEP_EPISODE_NAMESPACE,
            "ep",
            Some("old"),
            &ep,
            &ep.ended_at,
        )
        .await;
        for agent in ["alpha", "beta", "rogue"] {
            let current = EpisodeRecord {
                agent_id: agent.into(),
                session_key: "new".into(),
                ..ep.clone()
            };
            mem.commit_episode(&current).await.unwrap();
        }
        let scoped = AgentScopedMemory::new(std::sync::Arc::new(mem), "alpha", ["beta".into()]);
        assert_eq!(scoped.count().await.unwrap(), 2);
        assert!(scoped.list(None, Some("old")).await.unwrap().is_empty());
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

        legacy_row(
            &mem,
            &d1.agent_id,
            SLEEP_DAILY_DIGEST_NAMESPACE,
            &d1.day_key,
            None,
            &d1,
            &d1.created_at,
        )
        .await;
        assert_eq!(
            mem.load_daily_digests("agent_a", 1).await.unwrap(),
            std::slice::from_ref(&d1)
        );
        let pin = PinnedDigest {
            agent_id: "agent_a".into(),
            rendered_index: "legacy".into(),
            updated_at: d1.created_at.clone(),
            source_day_key: None,
        };
        legacy_row(
            &mem,
            &pin.agent_id,
            SLEEP_PINNED_DIGEST_NAMESPACE,
            SLEEP_PINNED_DIGEST_KEY,
            None,
            &pin,
            &pin.updated_at,
        )
        .await;
        assert_eq!(
            mem.load_pinned_digest("agent_a").await.unwrap(),
            Some(pin.clone())
        );
        let newer = PinnedDigest {
            rendered_index: "typed".into(),
            ..pin.clone()
        };
        mem.publish_pinned_digest(&newer).await.unwrap();
        assert_eq!(
            mem.load_pinned_digest("agent_a").await.unwrap(),
            Some(newer.clone())
        );

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
        assert_eq!(mem.rename_agent("agent_a", "renamed").await.unwrap(), 5);
        let renamed = mem.load_daily_digests("renamed", 10).await.unwrap();
        assert_eq!(renamed.len(), 2);
        assert!(renamed.iter().all(|d| d.agent_id == "renamed"));
        assert_eq!(
            mem.load_pinned_digest("renamed").await.unwrap(),
            Some(PinnedDigest {
                agent_id: "renamed".into(),
                ..newer
            })
        );
    }

    #[tokio::test]
    async fn purge_session_and_agent_session_erase_typed_episodes_and_lineage() {
        let (tmp, mem) = temp_memory();
        let pin = PinnedDigest {
            agent_id: "agent_a".into(),
            rendered_index: "keep pin".into(),
            updated_at: "2026-11-18T01:00:00Z".into(),
            source_day_key: None,
        };
        mem.publish_pinned_digest(&pin).await.unwrap();

        let mut ep_a_s1 = sample_episode(
            "ep-a-s1",
            "agent_a",
            "s1",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "a s1 v1",
        );
        let a_v1 = mem.commit_episode(&ep_a_s1).await.unwrap();
        ep_a_s1.summary = "a s1 v2".into();
        let a_v2 = mem.commit_episode(&ep_a_s1).await.unwrap();

        let ep_a_s2 = sample_episode(
            "ep-a-s2",
            "agent_a",
            "s2",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:30:00Z",
            "a s2 keep",
        );
        mem.commit_episode(&ep_a_s2).await.unwrap();

        let mut ep_b_s1 = sample_episode(
            "ep-b-s1",
            "agent_b",
            "s1",
            "2026-11-18T00:00:00Z",
            "2026-11-18T02:00:00Z",
            "b s1 v1",
        );
        let b_v1 = mem.commit_episode(&ep_b_s1).await.unwrap();
        ep_b_s1.summary = "b s1 v2".into();
        let b_v2 = mem.commit_episode(&ep_b_s1).await.unwrap();

        assert_eq!(
            mem.purge_session_for_agent("s1", "agent_a").await.unwrap(),
            2
        );
        assert!(
            mem.read_episodes_in_window("agent_a", None, None, Some("s1"), 10)
                .await
                .unwrap()
                .is_empty()
        );
        {
            let store = mem.store.lock();
            assert!(store.get_typed_record(&a_v1).unwrap().is_none());
            assert!(store.get_typed_record(&a_v2).unwrap().is_none());
            assert!(store.get_typed_record(&b_v2).unwrap().is_some());
        }
        assert_eq!(
            mem.read_episodes_in_window("agent_a", None, None, Some("s2"), 10)
                .await
                .unwrap(),
            vec![ep_a_s2.clone()]
        );
        assert_eq!(
            mem.load_pinned_digest("agent_a").await.unwrap(),
            Some(pin.clone())
        );
        assert_eq!(
            mem.read_episodes_in_window("agent_b", None, None, Some("s1"), 10)
                .await
                .unwrap(),
            vec![ep_b_s1]
        );

        assert_eq!(mem.purge_session("s1").await.unwrap(), 2);
        assert!(
            mem.read_episodes_in_window("agent_b", None, None, Some("s1"), 10)
                .await
                .unwrap()
                .is_empty()
        );
        {
            let store = mem.store.lock();
            assert!(store.get_typed_record(&b_v1).unwrap().is_none());
            assert!(store.get_typed_record(&b_v2).unwrap().is_none());
        }
        assert_eq!(
            mem.read_episodes_in_window("agent_a", None, None, Some("s2"), 10)
                .await
                .unwrap(),
            vec![ep_a_s2]
        );
        assert_eq!(mem.load_pinned_digest("agent_a").await.unwrap(), Some(pin));
        let conn = rusqlite::Connection::open_with_flags(
            tmp.path().join("memory/memorycore.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM typed_record_supersessions", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn shadowed_legacy_sleep_rows_are_not_double_counted_and_export_includes_typed() {
        let (_tmp, mem) = temp_memory();
        let mut pin = PinnedDigest {
            agent_id: "agent".into(),
            rendered_index: "legacy pin".into(),
            updated_at: "2026-11-18T01:00:00Z".into(),
            source_day_key: None,
        };
        let mut ep = sample_episode(
            "ep-1",
            "agent",
            "s1",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "legacy ep",
        );

        legacy_row(
            &mem,
            "agent",
            SLEEP_PINNED_DIGEST_NAMESPACE,
            SLEEP_PINNED_DIGEST_KEY,
            None,
            &pin,
            &pin.updated_at,
        )
        .await;
        legacy_row(
            &mem,
            "agent",
            SLEEP_EPISODE_NAMESPACE,
            &ep.episode_id,
            Some(&ep.session_key),
            &ep,
            &ep.ended_at,
        )
        .await;
        assert_eq!(mem.count().await.unwrap(), 2);
        assert_eq!(mem.count_agent("agent").await.unwrap(), 2);

        pin.rendered_index = "typed pin".into();
        ep.summary = "typed ep".into();
        mem.publish_pinned_digest(&pin).await.unwrap();
        mem.commit_episode(&ep).await.unwrap();

        assert_eq!(mem.count().await.unwrap(), 2);
        assert_eq!(mem.count_agent("agent").await.unwrap(), 2);
        let stats = mem.stats().await.unwrap();
        assert_eq!((stats.total_rows, stats.pinned_rows), (2, 1));
        assert_eq!(stats.by_category, vec![("other".into(), 2)]);

        let exported = mem.export_agent("agent").await.unwrap();
        assert_eq!(exported.len(), 2);
        let contents: Vec<&str> = exported.iter().map(|e| e.content.as_str()).collect();
        assert!(contents.iter().any(|c| c.contains("typed pin")));
        assert!(contents.iter().any(|c| c.contains("typed ep")));
        assert!(!contents.iter().any(|c| c.contains("legacy pin")));
        assert!(!contents.iter().any(|c| c.contains("legacy ep")));
    }

    #[tokio::test]
    async fn get_and_forget_for_agent_resolve_and_erase_typed_sleep_records_without_resurrecting_legacy()
     {
        let (tmp, mem) = temp_memory();
        let mut ep = sample_episode(
            "ep-lookup",
            "agent",
            "s1",
            "2026-11-18T00:00:00Z",
            "2026-11-18T01:00:00Z",
            "legacy ep",
        );
        legacy_row(
            &mem,
            "agent",
            SLEEP_EPISODE_NAMESPACE,
            &ep.episode_id,
            Some(&ep.session_key),
            &ep,
            &ep.ended_at,
        )
        .await;

        ep.summary = "v1 ep".into();
        let v1_id = mem.commit_episode(&ep).await.unwrap();
        ep.summary = "v2 ep".into();
        let v2_id = mem.commit_episode(&ep).await.unwrap();

        let pin = PinnedDigest {
            agent_id: "agent".into(),
            rendered_index: "typed lookup pin".into(),
            updated_at: "2026-11-18T02:00:00Z".into(),
            source_day_key: None,
        };
        mem.publish_pinned_digest(&pin).await.unwrap();

        let fetched = mem
            .get_for_agent("ep-lookup", "agent")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.id, v2_id);
        assert_eq!(fetched.key, "ep-lookup");
        assert_eq!(fetched.namespace, SLEEP_EPISODE_NAMESPACE);
        assert_eq!(
            fetched.category,
            MemoryCategory::Custom(SLEEP_EPISODE_NAMESPACE.into())
        );
        assert_eq!(fetched.session_id.as_deref(), Some("s1"));
        assert_eq!(fetched.agent_id.as_deref(), Some("agent"));
        assert!(fetched.superseded_by.is_none());
        let decoded: EpisodeRecord = serde_json::from_str(&fetched.content).unwrap();
        assert_eq!(decoded, ep);

        let fetched_pin = mem
            .get_for_agent(SLEEP_PINNED_DIGEST_KEY, "agent")
            .await
            .unwrap()
            .unwrap();
        let decoded_pin: PinnedDigest = serde_json::from_str(&fetched_pin.content).unwrap();
        assert_eq!(decoded_pin, pin);

        assert!(mem.forget_for_agent("ep-lookup", "agent").await.unwrap());
        assert!(
            mem.get_for_agent("ep-lookup", "agent")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            mem.read_episodes_in_window("agent", None, None, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        {
            let store = mem.store.lock();
            assert!(store.get_typed_record(&v1_id).unwrap().is_none());
            assert!(store.get_typed_record(&v2_id).unwrap().is_none());
        }
        let conn = rusqlite::Connection::open_with_flags(
            tmp.path().join("memory/memorycore.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM typed_record_supersessions", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
    }
}
