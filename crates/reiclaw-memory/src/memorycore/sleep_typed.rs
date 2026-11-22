//! The three immutable sleep contracts owned by the ReiClaw memory store.
use super::{KernelStoreKind, MemoryCore};
use crate::sleep_records::{DailyDigest, EpisodeRecord, PinnedDigest};
use crate::traits::{MemoryCategory, MemoryEntry};
use anyhow::{Context, Result, ensure};
use portable_kernel::{
    KernelPolicy, MemoryStore, RecordKind, RecordKindId, RecordRegistry, RecordWriter, TypedRecord,
    TypedRecordInput, TypedRecordQuery,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, path::Path, sync::Arc};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum SleepKind {
    Episode,
    Pinned,
    Daily,
}
use SleepKind::{Daily, Episode, Pinned};

const KINDS: [SleepKind; 3] = [Episode, Pinned, Daily];
const PAGE: usize = 1000;

impl SleepKind {
    fn id(self) -> RecordKindId {
        match self {
            Episode => RecordKindId {
                name: "reiclaw.sleep_episode".into(),
                version: 1,
            },
            Pinned => RecordKindId {
                name: "reiclaw.pinned_digest".into(),
                version: 1,
            },
            Daily => RecordKindId {
                name: "reiclaw.daily_digest".into(),
                version: 2,
            },
        }
    }

    fn read_ids(self) -> Vec<RecordKindId> {
        match self {
            Episode | Pinned => vec![self.id()],
            Daily => vec![
                RecordKindId {
                    name: "reiclaw.daily_digest".into(),
                    version: 2,
                },
                RecordKindId {
                    name: "reiclaw.daily_digest".into(),
                    version: 1,
                },
            ],
        }
    }

    fn matches_kind(self, kind: &RecordKindId) -> bool {
        match self {
            Episode | Pinned => *kind == self.id(),
            Daily => kind.name == "reiclaw.daily_digest" && matches!(kind.version, 1 | 2),
        }
    }

    pub(super) fn for_namespace(namespace: &str) -> Option<Self> {
        KINDS.into_iter().find(|kind| kind.namespace() == namespace)
    }

    fn namespace(self) -> &'static str {
        match self {
            Episode => "sleep_episode",
            Pinned => "sleep_pinned_digest",
            Daily => "sleep_daily_digest",
        }
    }

    pub(super) fn slot(self) -> &'static str {
        match self {
            Episode => "episode_id",
            Pinned => "slot",
            Daily => "day_key",
        }
    }

    fn time(self) -> &'static str {
        match self {
            Episode => "ended_at",
            Pinned => "updated_at",
            Daily => "created_at",
        }
    }

    fn definitions(self) -> Vec<RecordKind> {
        fn schema(properties: Value) -> Value {
            let mut required: Vec<_> = properties
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k)
                .collect();
            required.sort_unstable();
            json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
                "additionalProperties":false, "properties":properties, "required":required})
        }
        let string = json!({"type":"string"});
        let strings = json!({"type":"array","items":string});
        let mut keys = json!({"agent_id":string, self.time():string, self.slot():string});
        match self {
            Episode => keys["session_key"] = string.clone(),
            Pinned => keys["slot"] = json!({"const":"pinned_digest"}),
            Daily => {}
        }
        let key_schema = schema(keys);
        match self {
            Episode => vec![RecordKind {
                id: self.id(),
                key_schema,
                payload_schema: schema(json!({
                    "agent_id":string, "episode_id":string, "session_key":string, "started_at":string,
                    "ended_at":string, "summary":string, "up_to_message_id":{"type":["integer","null"]},
                    "why_and_tradeoffs":strings, "settled_decisions":strings, "rejected_options":strings,
                    "entities":strings, "source_kind":{"enum":["dialogue","delegation_digest","idle_flush","startup_recovery"]}
                })),
            }],
            Pinned => vec![RecordKind {
                id: self.id(),
                key_schema,
                payload_schema: schema(
                    json!({"agent_id":string, "rendered_index":string, "updated_at":string,
                    "source_day_key":{"type":["string","null"]}}),
                ),
            }],
            Daily => {
                let v1 = json!({"agent_id":string, "day_key":string, "window_start":string,
                    "window_end":string, "narrative":string, "created_at":string,
                    "settled_decisions":strings, "why_and_tradeoffs":strings, "latent_insights":strings,
                    "action_candidates":strings, "entities":strings, "owner_edited":{"type":"boolean"},
                    "action_decisions":{"type":"object","additionalProperties":{"enum":["pending","approved","dismissed"]}}
                });
                let mut v2 = v1.clone();
                if let Some(obj) = v2.as_object_mut() {
                    obj.insert("revision".into(), json!({"type":"integer","minimum":1}));
                    obj.insert(
                        "candidate_items".into(),
                        json!({"type":"array","items": schema(json!({"id":string,"text":string}))}),
                    );
                }
                vec![
                    RecordKind {
                        id: RecordKindId {
                            name: "reiclaw.daily_digest".into(),
                            version: 1,
                        },
                        key_schema: key_schema.clone(),
                        payload_schema: schema(v1),
                    },
                    RecordKind {
                        id: RecordKindId {
                            name: "reiclaw.daily_digest".into(),
                            version: 2,
                        },
                        key_schema,
                        payload_schema: schema(v2),
                    },
                ]
            }
        }
    }

    pub(super) fn query(self, agent: Option<&str>) -> TypedRecordQuery {
        TypedRecordQuery {
            kind: Some(self.id()),
            key_filters: agent.map(agent_filter).unwrap_or_default(),
            ..Default::default()
        }
    }

    pub(super) fn queries(self, agent: Option<&str>) -> Vec<TypedRecordQuery> {
        let key_filters = agent.map(agent_filter).unwrap_or_default();
        self.read_ids()
            .into_iter()
            .map(|kind_id| TypedRecordQuery {
                kind: Some(kind_id),
                key_filters: key_filters.clone(),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn current(
        self,
        store: &MemoryStore,
        agent: &str,
        slot: &str,
    ) -> Result<Option<TypedRecord>> {
        let mut rows = Vec::new();
        for mut query in self.queries(Some(agent)) {
            query.key_filters.insert(self.slot().into(), slot.into());
            query.limit = 2;
            rows.extend(store.list_typed_records(&query)?);
            ensure!(
                rows.len() <= 1,
                "multiple active revisions for a sleep record"
            );
        }
        Ok(rows.pop())
    }

    fn keys(self, payload: &Value) -> Result<Value> {
        let time = payload[self.time()]
            .as_str()
            .context("missing sleep record time")?;
        let instant = reiclaw_api::time::parse_rfc3339(time)?;
        let mut keys = json!({"agent_id":payload["agent_id"],
            self.slot():payload[self.slot()], self.time():instant.to_string()});
        match self {
            Episode => keys["session_key"] = payload["session_key"].clone(),
            Pinned => keys["slot"] = json!("pinned_digest"),
            Daily => {}
        }
        Ok(keys)
    }

    pub(super) fn validate_payload(self, payload: &Value) -> Result<()> {
        match self {
            Episode => serde_json::from_value::<EpisodeRecord>(payload.clone())?.validate(),
            Pinned => serde_json::from_value::<PinnedDigest>(payload.clone())?.validate(),
            Daily => serde_json::from_value::<DailyDigest>(payload.clone())?.validate(),
        }
    }

    pub(super) fn verified_payload(self, record: TypedRecord) -> Result<Value> {
        self.validate_payload(&record.payload)?;
        ensure!(
            self.matches_kind(&record.kind) && record.keys == self.keys(&record.payload)?,
            "sleep record keys disagree with its payload"
        );
        Ok(record.payload)
    }

    pub(super) fn append(
        self,
        store: &mut MemoryStore,
        payload: Value,
        predecessor: Option<&str>,
    ) -> Result<String> {
        let input = TypedRecordInput {
            id: Uuid::new_v4().to_string(),
            kind: self.id(),
            keys: self.keys(&payload)?,
            payload,
            links: Vec::new(),
        };
        Ok(store
            .append_typed_record(&writer()?, &input, predecessor)?
            .record
            .id)
    }
}

fn writer() -> Result<RecordWriter> {
    let kind_ids: Vec<RecordKindId> = KINDS.into_iter().flat_map(SleepKind::read_ids).collect();
    Ok(RecordWriter::from_host_config(
        "reiclaw-sleep",
        KernelStoreKind::Memory.role(),
        kind_ids,
    )?)
}

pub(super) fn register(store: MemoryStore) -> Result<MemoryStore> {
    let definitions: Vec<RecordKind> = KINDS.into_iter().flat_map(SleepKind::definitions).collect();
    let registry = RecordRegistry::new(definitions)?;
    let mut store = store.with_kernel_policy(KernelPolicy {
        record_registry: Some(Arc::new(registry)),
        ..Default::default()
    });
    store
        .register_record_kinds()
        .context("registering ReiClaw sleep record kinds")?;
    Ok(store)
}

// Kernel appends and erasures each own a transaction. Serialize multi-step
// host operations across independently opened handles as well as within one handle.
fn lock_file(db: &Path) -> Result<File> {
    Ok(File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(db.with_extension("sleep.lock"))?)
}

pub(super) fn write_guard(db: &Path) -> Result<File> {
    let file = lock_file(db)?;
    file.lock().context("locking sleep record writes")?;
    Ok(file)
}

pub(super) fn read_guard(db: &Path) -> Result<File> {
    let file = lock_file(db)?;
    file.lock_shared().context("locking sleep record reads")?;
    Ok(file)
}

fn agent_filter(agent: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("agent_id".into(), agent.into())])
}

pub(super) fn purge(store: &mut MemoryStore, agent: &str) -> Result<usize> {
    Ok(store
        .purge_typed_records(&writer()?, &agent_filter(agent))?
        .erased_records)
}

// v44 has inclusive time bounds but no ID cursor. Leave the final timestamp
// on a full page for the next query, so a tie never loses a record.
fn visit_by_append_time(
    store: &MemoryStore,
    mut query: TypedRecordQuery,
    mut visit: impl FnMut(TypedRecord) -> Result<()>,
) -> Result<()> {
    query.limit = PAGE;
    loop {
        let page = store.list_typed_records(&query)?;
        let next = page
            .last()
            .filter(|_| page.len() == PAGE)
            .map(|r| r.created_at.clone());
        ensure!(
            next.is_none() || next != query.until,
            "v44 cannot page 1000 sleep revisions with the same append timestamp"
        );
        for record in page {
            if Some(&record.created_at) == next.as_ref() {
                break;
            }
            visit(record)?;
        }
        if next.is_none() {
            return Ok(());
        }
        query.until = next;
    }
}

pub(super) fn visit_active(
    store: &MemoryStore,
    agent: Option<&str>,
    mut visit: impl FnMut(SleepKind, TypedRecord) -> Result<()>,
) -> Result<()> {
    for kind in KINDS {
        for query in kind.queries(agent) {
            visit_by_append_time(store, query, |row| visit(kind, row))?;
        }
    }
    Ok(())
}

pub(super) fn active_slots(
    store: &MemoryStore,
    agent: Option<&str>,
) -> Result<std::collections::HashSet<(String, SleepKind, String)>> {
    let mut slots = std::collections::HashSet::new();
    visit_active(store, agent, |kind, record| {
        let agent_id = record.keys["agent_id"]
            .as_str()
            .context("missing sleep agent")?
            .to_owned();
        let slot = record.keys[kind.slot()]
            .as_str()
            .context("missing sleep slot")?
            .to_owned();
        slots.insert((agent_id, kind, slot));
        Ok(())
    })?;
    Ok(slots)
}

fn to_memory_entry(kind: SleepKind, row: TypedRecord) -> Result<MemoryEntry> {
    let id = row.id.clone();
    let keys = row.keys.clone();
    let payload = kind.verified_payload(row)?;
    let agent = keys["agent_id"]
        .as_str()
        .context("missing sleep agent")?
        .to_owned();
    let key = match kind {
        Pinned => "sleep:pinned_digest",
        _ => keys[kind.slot()].as_str().context("missing sleep slot")?,
    }
    .to_owned();
    Ok(MemoryEntry {
        id,
        key,
        content: serde_json::to_string(&payload)?,
        category: MemoryCategory::Custom(kind.namespace().to_owned()),
        timestamp: keys[kind.time()]
            .as_str()
            .context("missing sleep time")?
            .into(),
        session_id: keys["session_key"].as_str().map(str::to_owned),
        score: None,
        namespace: kind.namespace().into(),
        importance: Some(0.8),
        superseded_by: None,
        kind: None,
        pinned: kind == Pinned,
        tenant_id: None,
        agent_alias: Some(agent.clone()),
        agent_id: Some(agent),
    })
}

fn merge_entries(entries: &mut Vec<MemoryEntry>, typed: Vec<MemoryEntry>) {
    if typed.is_empty() {
        return;
    }
    let shadows: std::collections::HashSet<(String, String, String)> = typed
        .iter()
        .filter_map(|e| {
            e.agent_id
                .as_ref()
                .map(|a| (a.clone(), e.namespace.clone(), e.key.clone()))
        })
        .collect();
    entries.retain(|e| {
        !e.agent_id.as_ref().is_some_and(|a| {
            e.category == MemoryCategory::Custom(e.namespace.clone())
                && shadows.contains(&(a.clone(), e.namespace.clone(), e.key.clone()))
        })
    });
    entries.extend(typed);
}

// Generic listing is also the scoped decorator's visibility-aware count.
pub(super) fn extend_visible_entries(
    store: &MemoryStore,
    agent: Option<&str>,
    entries: &mut Vec<MemoryEntry>,
    category: Option<&MemoryCategory>,
) -> Result<()> {
    let mut typed = Vec::new();
    for kind in KINDS {
        let cat = MemoryCategory::Custom(kind.namespace().to_owned());
        if category.is_some_and(|c| c != &cat) {
            continue;
        }
        for query in kind.queries(agent) {
            visit_by_append_time(store, query, |row| {
                typed.push(to_memory_entry(kind, row)?);
                Ok(())
            })?;
        }
    }
    merge_entries(entries, typed);
    Ok(())
}

pub(super) fn extend_key_entries(
    store: &MemoryStore,
    key: &str,
    agent: Option<&str>,
    entries: &mut Vec<MemoryEntry>,
) -> Result<()> {
    if key.starts_with(crate::SOUL_KEY_PREFIX) {
        return Ok(());
    }
    let mut typed = Vec::new();
    for kind in [Daily, Episode, Pinned] {
        let slot = match kind {
            Pinned if key == "sleep:pinned_digest" => "pinned_digest",
            Pinned => continue,
            Episode | Daily => key,
        };
        for mut query in kind.queries(agent) {
            query.key_filters.insert(kind.slot().into(), slot.into());
            visit_by_append_time(store, query, |row| {
                typed.push(to_memory_entry(kind, row)?);
                Ok(())
            })?;
        }
    }
    merge_entries(entries, typed);
    Ok(())
}

pub(super) fn purge_slots(
    store: &mut MemoryStore,
    kind: SleepKind,
    query: TypedRecordQuery,
) -> Result<usize> {
    let mut slots = Vec::new();
    for kind_id in kind.read_ids() {
        let mut q = query.clone();
        q.kind = Some(kind_id);
        visit_by_append_time(store, q, |row| {
            let mut filters = agent_filter(
                row.keys["agent_id"]
                    .as_str()
                    .context("missing sleep agent")?,
            );
            filters.insert(
                kind.slot().into(),
                row.keys[kind.slot()]
                    .as_str()
                    .context("missing sleep slot")?
                    .into(),
            );
            slots.push(filters);
            Ok(())
        })?;
    }
    let mut deleted = 0;
    let writer = writer()?;
    for slot in slots {
        deleted += store.purge_typed_records(&writer, &slot)?.erased_records;
    }
    Ok(deleted)
}

pub(super) fn copy_for_rename(store: &mut MemoryStore, from: &str, to: &str) -> Result<usize> {
    let mut pending = Vec::new();
    let mut moved = 0;
    for kind in KINDS {
        for query in kind.queries(Some(from)) {
            visit_by_append_time(store, query, |record| {
                let mut payload = kind.verified_payload(record)?;
                payload["agent_id"] = json!(to);
                if kind == Daily {
                    let digest: DailyDigest = serde_json::from_value(payload)?;
                    payload = serde_json::to_value(digest)?;
                }
                kind.validate_payload(&payload)?;
                let keys = kind.keys(&payload)?;
                let slot = keys[kind.slot()].as_str().context("missing sleep slot")?;
                if let Some(existing) = kind.current(store, to, slot)? {
                    let existing_payload = kind.verified_payload(existing)?;
                    let same = if kind == Daily {
                        serde_json::from_value::<DailyDigest>(existing_payload)?
                            == serde_json::from_value::<DailyDigest>(payload.clone())?
                    } else {
                        existing_payload == payload
                    };
                    ensure!(same, "destination sleep record already exists");
                } else {
                    ensure!(
                        MemoryCore::legacy_sleep_slot(store, kind, to, slot)?.is_none(),
                        "destination legacy sleep record already exists"
                    );
                    pending.push((kind, payload));
                }
                moved += 1;
                Ok(())
            })?;
        }
    }
    for (kind, payload) in pending {
        kind.append(store, payload, None)?;
    }
    Ok(moved)
}

/// Fetch the requested prefix, including the whole final time bucket for
/// callers with secondary ordering. Indexed times are canonical UTC strings.
pub(super) fn ordered(
    store: &MemoryStore,
    kind: SleepKind,
    query: TypedRecordQuery,
    limit: usize,
    include_ties: bool,
) -> Result<Vec<TypedRecord>> {
    let mut result = Vec::new();
    let time = kind.time();
    for kind_id in kind.read_ids() {
        let mut q = query.clone();
        q.kind = Some(kind_id.clone());
        q.time_key = Some(time.into());
        let mut version_rows = Vec::new();
        while version_rows.len() < limit {
            let remaining = limit - version_rows.len();
            q.limit = remaining
                .saturating_add(usize::from(include_ties))
                .min(PAGE);
            let mut page = store.list_typed_records(&q)?;
            if page.len() < q.limit || (!include_ties && page.len() == remaining) {
                version_rows.extend(page);
                break;
            }
            let boundary = page
                .last()
                .and_then(|r| r.keys[time].as_str())
                .context("missing sleep time key")?
                .to_owned();
            page.retain(|r| r.keys[time] != boundary);
            version_rows.extend(page);
            if version_rows.len() >= limit {
                break;
            }
            // Use the kernel append clock to walk even a large domain-time tie.
            let mut bucket = TypedRecordQuery {
                kind: Some(kind_id.clone()),
                ..Default::default()
            };
            bucket.key_filters = q.key_filters.clone();
            bucket.key_filters.insert(time.into(), boundary.clone());
            visit_by_append_time(store, bucket, |r| {
                version_rows.push(r);
                Ok(())
            })?;
            let instant: jiff::Timestamp = boundary.parse()?;
            let Ok(before) = instant.checked_sub(jiff::SignedDuration::from_nanos(1)) else {
                break;
            };
            if q.since
                .as_deref()
                .map(str::parse::<jiff::Timestamp>)
                .transpose()?
                .is_some_and(|s| before < s)
            {
                break;
            }
            q.until = Some(before.to_string());
        }
        result.extend(version_rows);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_kernel::DbOpenContext;

    #[test]
    fn registry_conflict_refuses_memorycore_open() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("memory/memorycore.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let store = MemoryStore::open_with_context(
            db.to_str().unwrap(),
            &KernelStoreKind::Memory.context(DbOpenContext::create_fresh()),
        )
        .unwrap();
        let mut kind = Pinned.definitions().into_iter().next().unwrap();
        kind.key_schema["properties"]["slot"]["const"] = json!("different");
        let mut store = store.with_kernel_policy(KernelPolicy {
            record_registry: Some(Arc::new(RecordRegistry::new(vec![kind]).unwrap())),
            ..Default::default()
        });
        store.register_record_kinds().unwrap();
        drop(store);
        let error = MemoryCore::new("test", tmp.path()).err().unwrap();
        assert!(
            format!("{error:#}")
                .contains("record kind definition conflict for reiclaw.pinned_digest@1")
        );
    }
}
