use super::tools::*;
use super::*;
use async_trait::async_trait;
use reiclaw_api::attribution::{
    Attributable, MemoryKind as AttrMemoryKind, ModelProviderKind, ProviderKind, Role,
};
use reiclaw_api::memory_traits::{
    ActionCandidateStatus, PINNED_DIGEST_MAX_BYTES, PinnedDigest, validate_agent_id,
    validate_non_soul_namespace,
};
use reiclaw_api::model_provider::{ChatRequest, ChatResponse, ProviderCapabilities, ToolCall};
use reiclaw_config::attention::AttentionConfig;
use reiclaw_config::multi_agent::{AgentMemoryConfig, MemoryBackendKind};
use reiclaw_config::schema::{
    AliasedAgentConfig, ModelProviderConfig, ModelRouteConfig, OllamaModelProviderConfig,
    OpenAIModelProviderConfig,
};
use reiclaw_infra::session_sqlite::SqliteSessionBackend;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

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
        "memorycore"
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
            timestamp: reiclaw_api::time::format_rfc3339(jiff::Timestamp::now()),
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
            timestamp: reiclaw_api::time::format_rfc3339(jiff::Timestamp::now()),
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
        let since_ts = since.map(reiclaw_api::time::parse_rfc3339).transpose()?;
        let until_ts = until.map(reiclaw_api::time::parse_rfc3339).transpose()?;
        let mut out: Vec<EpisodeRecord> = self
            .episodes
            .lock()
            .iter()
            .filter(|e| e.agent_id == agent_id)
            .filter(|e| session_id.is_none_or(|sid| e.session_key == sid))
            .filter(|e| {
                let Ok(end) = reiclaw_api::time::parse_rfc3339(&e.ended_at) else {
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
            timestamp: reiclaw_api::time::format_rfc3339(jiff::Timestamp::now()),
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
                backend: MemoryBackendKind::MemoryCore,
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
async fn cold_window_promotion_reloads_session_window_before_watermark_check() {
    use reiclaw_infra::session_backend::{SessionCheckpoint, SessionWindow};
    use std::sync::atomic::AtomicBool;

    struct AgedStagedBackend {
        inner: Arc<dyn SessionBackend>,
        aged_once: AtomicBool,
    }

    impl SessionBackend for AgedStagedBackend {
        fn load(&self, session_key: &str) -> Vec<ChatMessage> {
            self.inner.load(session_key)
        }
        fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()> {
            self.inner.append(session_key, message)
        }
        fn remove_last(&self, session_key: &str) -> std::io::Result<bool> {
            self.inner.remove_last(session_key)
        }
        fn list_sessions(&self) -> Vec<String> {
            self.inner.list_sessions()
        }
        fn stage_checkpoint(
            &self,
            session_key: &str,
            up_to_message_id: i64,
            digest: &str,
        ) -> std::io::Result<SessionCheckpoint> {
            self.inner
                .stage_checkpoint(session_key, up_to_message_id, digest)
        }
        fn promote_staged_checkpoint(
            &self,
            session_key: &str,
        ) -> std::io::Result<Option<SessionCheckpoint>> {
            self.inner.promote_staged_checkpoint(session_key)
        }
        fn load_session_window(&self, session_key: &str) -> std::io::Result<SessionWindow> {
            let mut w = self.inner.load_session_window(session_key)?;
            if !self.aged_once.swap(true, Ordering::SeqCst)
                && let Some(ref mut staged) = w.staged_checkpoint
            {
                let old = jiff::Timestamp::from_second(
                    jiff::Timestamp::now()
                        .as_second()
                        .saturating_sub(COLD_WINDOW_GAP_SECS + 60),
                )
                .unwrap();
                staged.updated_at = old;
            }
            Ok(w)
        }
    }

    let (_dir, store, raw_sessions) = sleep_fixture();
    let mem: Arc<dyn Memory> = store.clone();
    raw_sessions
        .append("cold_sess", &ChatMessage::user("Earlier turn"))
        .unwrap();
    raw_sessions
        .append("cold_sess", &ChatMessage::assistant("Earlier reply"))
        .unwrap();
    raw_sessions
        .stage_checkpoint("cold_sess", 2, "Cold staged digest")
        .unwrap();

    let wrapped: Arc<dyn SessionBackend> = Arc::new(AgedStagedBackend {
        inner: Arc::clone(&raw_sessions),
        aged_once: AtomicBool::new(false),
    });

    // Unreachable provider URI: if window wasn't reloaded after promotion,
    // `delta_messages` would still look non-empty and attempt an LLM call.
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

    maybe_compact_ws_turn(
        &config,
        Some(&wrapped),
        Some(&mem),
        "nova",
        "cold_sess",
        "openai.primary",
        "gpt-5",
        1000,
        Some(800),
    )
    .await
    .unwrap();

    let reloaded = raw_sessions.load_session_window("cold_sess").unwrap();
    assert!(reloaded.staged_checkpoint.is_none());
    assert!(reloaded.delta_messages.is_empty());
    assert_eq!(reloaded.active_checkpoint.unwrap().up_to_message_id, 2);
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
    let before_cut = due_dream_window("2026-11-18T03:59:00Z".parse().unwrap(), tz.clone()).unwrap();
    assert_eq!(before_cut.day_key, "2026-11-17");

    let at_cut = due_dream_window("2026-11-18T04:00:00Z".parse().unwrap(), tz.clone()).unwrap();
    assert_eq!(at_cut.day_key, "2026-11-18");
    assert_eq!(
        reiclaw_api::time::parse_rfc3339(&at_cut.window_start).unwrap(),
        "2026-11-17T04:00:00Z".parse::<jiff::Timestamp>().unwrap()
    );
    assert_eq!(
        reiclaw_api::time::parse_rfc3339(&at_cut.window_end).unwrap(),
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

#[cfg(feature = "memory-core")]
#[tokio::test]
async fn memorycore_end_to_end_sleep_compactor_parity() {
    let dir = TempDir::new().unwrap();
    let mc = Arc::new(reiclaw_memory::MemoryCore::new("memorycore", dir.path()).unwrap());
    let mem: Arc<dyn Memory> = mc;
    let mut cfg = MemoryConfig::default();
    cfg.types.enabled = true;
    cfg.consolidation_extract_facts = true;

    let provider = ScriptedNativeProvider::new(
        "memorycore-parity",
        vec![ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "episode_commit".into(),
                arguments: serde_json::json!({
                    "summary": "Adopted ADR-025 sleep compactor on MemoryCore",
                    "why_and_tradeoffs": ["Single SQLite store avoids handle drift"],
                    "settled_decisions": ["Use MemoryCore sleep traits"],
                    "rejected_options": ["Dual SQLite stores"],
                    "primary_memory_update": "MemoryCore hosts sleep records",
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
        "sess_mc",
        "Let's keep sleep records in MemoryCore",
        "Agreed, single MemoryCore handle.",
    )
    .await
    .unwrap();

    let eps = mem
        .read_episodes_in_window("nova", None, None, Some("sess_mc"), 5)
        .await
        .unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(
        eps[0].settled_decisions,
        vec!["Use MemoryCore sleep traits"]
    );
    let cores = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
    assert!(
        cores
            .iter()
            .any(|c| c.content == "MemoryCore hosts sleep records")
    );
    assert!(
        cores
            .iter()
            .any(|c| c.content == "Dual SQLite stores were rejected")
    );
}
