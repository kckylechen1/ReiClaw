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
        let name = match self {
            Episode => "reiclaw.sleep_episode",
            Pinned => "reiclaw.pinned_digest",
            Daily => "reiclaw.daily_digest",
        };
        RecordKindId {
            name: name.into(),
            version: 1,
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

    fn definition(self) -> RecordKind {
        let string = json!({"type":"string"});
        let strings = json!({"type":"array","items":string});
        let mut keys = json!({"agent_id":string, self.time():string, self.slot():string});
        match self {
            Episode => keys["session_key"] = string.clone(),
            Pinned => keys["slot"] = json!({"const":"pinned_digest"}),
            Daily => {}
        }
        let payload = match self {
            Episode => json!({
                "agent_id":string, "episode_id":string, "session_key":string, "started_at":string,
                "ended_at":string, "summary":string, "up_to_message_id":{"type":["integer","null"]},
                "why_and_tradeoffs":strings, "settled_decisions":strings, "rejected_options":strings,
                "entities":strings, "source_kind":{"enum":["dialogue","delegation_digest","idle_flush","startup_recovery"]}
            }),
            Pinned => json!({"agent_id":string, "rendered_index":string, "updated_at":string,
                "source_day_key":{"type":["string","null"]}}),
            Daily => json!({"agent_id":string, "day_key":string, "window_start":string,
                "window_end":string, "narrative":string, "created_at":string,
                "settled_decisions":strings, "why_and_tradeoffs":strings, "latent_insights":strings,
                "action_candidates":strings, "entities":strings, "owner_edited":{"type":"boolean"},
                "action_decisions":{"type":"object","additionalProperties":{"enum":["pending","approved","dismissed"]}}
            }),
        };
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
        RecordKind {
            id: self.id(),
            key_schema: schema(keys),
            payload_schema: schema(payload),
        }
    }

    pub(super) fn query(self, agent: Option<&str>) -> TypedRecordQuery {
        TypedRecordQuery {
            kind: Some(self.id()),
            key_filters: agent.map(agent_filter).unwrap_or_default(),
            ..Default::default()
        }
    }

    pub(super) fn current(
        self,
        store: &MemoryStore,
        agent: &str,
        slot: &str,
    ) -> Result<Option<TypedRecord>> {
        let mut query = self.query(Some(agent));
        query.key_filters.insert(self.slot().into(), slot.into());
        query.limit = 2;
        let mut rows = store.list_typed_records(&query)?;
        ensure!(
            rows.len() <= 1,
            "multiple active revisions for a sleep record"
        );
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
            record.kind == self.id() && record.keys == self.keys(&record.payload)?,
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
    Ok(RecordWriter::from_host_config(
        "reiclaw-sleep",
        KernelStoreKind::Memory.role(),
        KINDS.map(SleepKind::id).into(),
    )?)
}

pub(super) fn register(store: MemoryStore) -> Result<MemoryStore> {
    let registry = RecordRegistry::new(KINDS.map(SleepKind::definition).into())?;
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
        visit_by_append_time(store, kind.query(agent), |row| visit(kind, row))?;
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
        visit_by_append_time(store, kind.query(agent), |row| {
            typed.push(to_memory_entry(kind, row)?);
            Ok(())
        })?;
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
        let mut query = kind.query(agent);
        query.key_filters.insert(kind.slot().into(), slot.into());
        visit_by_append_time(store, query, |row| {
            typed.push(to_memory_entry(kind, row)?);
            Ok(())
        })?;
    }
    merge_entries(entries, typed);
    Ok(())
}

pub(super) fn purge_slots(
    store: &mut MemoryStore,
    kind: SleepKind,
    mut query: TypedRecordQuery,
) -> Result<usize> {
    let mut slots = Vec::new();
    query.kind = Some(kind.id());
    visit_by_append_time(store, query, |row| {
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
        visit_by_append_time(store, kind.query(Some(from)), |record| {
            let mut payload = kind.verified_payload(record)?;
            payload["agent_id"] = json!(to);
            kind.validate_payload(&payload)?;
            let keys = kind.keys(&payload)?;
            let slot = keys[kind.slot()].as_str().context("missing sleep slot")?;
            if let Some(existing) = kind.current(store, to, slot)? {
                ensure!(
                    kind.verified_payload(existing)? == payload,
                    "destination sleep record already exists"
                );
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
    mut query: TypedRecordQuery,
    limit: usize,
    include_ties: bool,
) -> Result<Vec<TypedRecord>> {
    let mut result = Vec::new();
    let time = kind.time();
    query.time_key = Some(time.into());
    while result.len() < limit {
        let remaining = limit - result.len();
        query.limit = remaining
            .saturating_add(usize::from(include_ties))
            .min(PAGE);
        let mut page = store.list_typed_records(&query)?;
        if page.len() < query.limit || (!include_ties && page.len() == remaining) {
            result.extend(page);
            break;
        }
        let boundary = page
            .last()
            .and_then(|r| r.keys[time].as_str())
            .context("missing sleep time key")?
            .to_owned();
        page.retain(|r| r.keys[time] != boundary);
        result.extend(page);
        if result.len() >= limit {
            break;
        }
        // Use the kernel append clock to walk even a large domain-time tie.
        let mut bucket = kind.query(None);
        bucket.key_filters = query.key_filters.clone();
        bucket.key_filters.insert(time.into(), boundary.clone());
        visit_by_append_time(store, bucket, |r| {
            result.push(r);
            Ok(())
        })?;
        let instant: jiff::Timestamp = boundary.parse()?;
        let Ok(before) = instant.checked_sub(jiff::SignedDuration::from_nanos(1)) else {
            break;
        };
        if query
            .since
            .as_deref()
            .map(str::parse::<jiff::Timestamp>)
            .transpose()?
            .is_some_and(|s| before < s)
        {
            break;
        }
        query.until = Some(before.to_string());
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
        let mut kind = Pinned.definition();
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
