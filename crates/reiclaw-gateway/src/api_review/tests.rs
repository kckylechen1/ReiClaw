use super::*;
use axum::routing::post;
use reiclaw_api::memory_traits::DailyDigest;
use reiclaw_api::model_provider::ModelProvider;
use reiclaw_memory::companion::reflection::{OwnerMessages, reflect};
use reiclaw_memory::companion::{SoulStory, StoryKind, UserModelKind};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct FakeReflection;
#[async_trait::async_trait]
impl ModelProvider for FakeReflection {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _input: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        Ok(r#"{"proposals":[{"layer":"story","story_kind":"bond","proposal":"Shared shorthand"}],"user_model_candidates":[{"kind":"preference","statement":"Concise replies","semantic_key":"communication.short","evidence_indices":[0]}]}"#.into())
    }
}
impl reiclaw_api::attribution::Attributable for FakeReflection {
    fn role(&self) -> reiclaw_api::attribution::Role {
        use reiclaw_api::attribution::*;
        Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
    }
    fn alias(&self) -> &str {
        "reflection-fixture"
    }
}
fn fixture() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = reiclaw_config::schema::Config {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    config.agents.insert(
        "nova".into(),
        reiclaw_config::schema::AliasedAgentConfig {
            enabled: true,
            ..Default::default()
        },
    );
    config.nodes.auth_token = Some("node-token".into());
    let mut state = crate::api::test_state(config);
    state.pairing = Arc::new(reiclaw_runtime::security::pairing::PairingGuard::new(
        false,
        &["operator-token".into()],
    ));
    (dir, state)
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn server(state: AppState) -> (Server, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = routes()
        .route(
            "/api/user-model/candidates/{id}/review",
            post(crate::api_user_model::review_candidate),
        )
        .route(
            "/api/soul/proposals/{id}/resolve",
            post(crate::api_soul::post_resolve_proposal),
        )
        .with_state(state);
    let handle = reiclaw_log::spawn!(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (Server(handle), address)
}
async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let raw = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut response = Vec::new(); stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (header, body) = response.split_once("\r\n\r\n").unwrap();
        let status = header.split_whitespace().nth(1).unwrap().parse::<u16>().unwrap();
        (StatusCode::from_u16(status).unwrap(), serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({"raw":body})))
    }).await.unwrap()
}
const OPERATOR: Option<&str> = Some("operator-token");

#[tokio::test]
async fn actual_http_reflection_inbox_review_reword_narrow_dismiss_and_reopen() {
    let (dir, state) = fixture();
    let soul = SoulProfileStore::open(dir.path()).unwrap();
    soul.ensure_seeded("nova", "Nova", 100).unwrap();
    let before = soul.profile("nova").unwrap();
    let user = UserModelStore::open(dir.path()).unwrap();
    let messages = OwnerMessages {
        messages: vec![reiclaw_api::review::ReflectionMessage {
            source: reiclaw_api::review::UserMessageSource::Operator,
            session_id: "session-a".into(),
            at_unix: 101,
            text: "Please use concise replies and our shorthand".into(),
        }],
    };
    let receipt = reflect(
        &soul,
        "nova",
        &user,
        || Some(Default::default()),
        &messages,
        || {
            Ok((
                Box::new(FakeReflection) as Box<dyn ModelProvider>,
                "fake".into(),
            ))
        },
        100,
        102,
    )
    .await
    .unwrap();
    assert_eq!(
        (
            receipt.proposals_created,
            receipt.user_model_candidates_created
        ),
        (1, 1)
    );
    assert_eq!(soul.profile("nova").unwrap(), before);
    assert!(user.active_heads(None).unwrap().is_empty());
    let candidate = user.list_pending_candidates().unwrap().remove(0);
    let (_server, address) = server(state).await;
    for token in [None, Some("node-token"), Some("bridge-token")] {
        assert_eq!(
            request(address, "GET", "/api/review/inbox", token, None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, inbox) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inbox["total"], 3);
    let items = inbox["items"].as_array().unwrap();
    let proposal = items.iter().find(|i| i["kind"] == "soul_proposal").unwrap();
    let user_item = items
        .iter()
        .find(|i| i["kind"] == "user_model_candidate")
        .unwrap();
    let receipt = items
        .iter()
        .find(|i| i["kind"] == "reflection_receipt")
        .unwrap();
    assert_eq!(receipt["item"]["user_model_candidates_created"], 1);
    // ADR-018 §4.3: the receipt carries the drift against the (default) anchor.
    assert_eq!(receipt["item"]["drift"]["anchor"]["source"], "default");
    assert_eq!(receipt["item"]["drift"]["anchor"]["set_at_unix"], 100);
    assert_eq!(
        receipt["item"]["drift"]["dials"].as_array().unwrap().len(),
        5
    );
    assert_eq!(receipt["item"]["drift"]["story_added"], 0);
    assert_eq!(user_item["item"]["evidence"], candidate.evidence);
    assert_eq!(request(address,"POST",proposal["review_url"].as_str().unwrap(),OPERATOR,Some(serde_json::json!({"agent":"nova","resolution":"accepted","final_text":"Owner chosen shorthand"}))).await.0,StatusCode::OK);
    let review_path = user_item["review_url"].as_str().unwrap();
    let body = serde_json::json!({"action":"narrow","narrowed_scope":"session:session-a","final_text":"Keep replies brief in this session"});
    assert_eq!(
        request(address, "POST", review_path, OPERATOR, Some(body.clone()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(address, "POST", review_path, OPERATOR, Some(body))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let reopened = UserModelStore::open(dir.path()).unwrap();
    let heads = reopened.active_heads(None).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].statement, "Keep replies brief in this session");
    assert_eq!(heads[0].scope, "session:session-a");
    assert_eq!(
        heads[0].source_candidate.as_deref(),
        Some(candidate.id.as_str())
    );
    let (original, decisions) = reopened.candidate_history(&candidate.id).unwrap().unwrap();
    assert_eq!(original, candidate);
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        SoulProfileStore::open(dir.path())
            .unwrap()
            .profile("nova")
            .unwrap()
            .story
            .unwrap()
            .value,
        SoulStory {
            entries: vec![reiclaw_memory::companion::StoryEntry {
                kind: StoryKind::Bond,
                text: "Owner chosen shorthand".into()
            }]
        }
    );
    let rejected = user
        .record_observation(
            UserModelKind::Habit,
            "Unwanted suggestion",
            "habit.example",
            "[]",
            104,
        )
        .unwrap();
    assert_eq!(
        request(
            address,
            "POST",
            &format!("/api/user-model/candidates/{}/review", rejected.id),
            OPERATOR,
            Some(serde_json::json!({"action":"reject"}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(reopened.active_heads(None).unwrap().len(), 1);
    assert_eq!(
        reopened.candidate_history(&rejected.id).unwrap().unwrap().0,
        rejected
    );
    let (_, after) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(after["total"], 1);
    assert_eq!(after["items"][0]["kind"], "reflection_receipt");
}

#[tokio::test]
async fn unavailable_store_and_invalid_queries_return_errors() {
    let (dir, state) = fixture();
    let (_server, address) = server(state).await;
    for suffix in ["?limit=0", "?limit=201", "?offset=bad", "?unknown=1"] {
        assert_eq!(
            request(
                address,
                "GET",
                &format!("/api/review/inbox{suffix}"),
                OPERATOR,
                None
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            address,
            "GET",
            "/api/review/inbox?agent=unknown",
            OPERATOR,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    std::fs::create_dir(dir.path().join("soul.db")).unwrap();
    assert_eq!(
        request(address, "GET", "/api/review/inbox", OPERATOR, None)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn note_owner_correction_enters_http_inbox_and_only_operator_acceptance_applies() {
    use reiclaw_api::review::{
        OWNER_CORRECTION_CONTEXT, OwnerCorrectionContext, OwnerCorrectionResolver,
        UserMessageIngress, UserMessageSource,
    };
    use reiclaw_api::tool::Tool;
    let (dir, state) = fixture();
    let live_config = state.config.clone();
    let tool =
        reiclaw_tools::note_owner_correction::NoteOwnerCorrectionTool::new("nova", move || {
            let config = live_config.read();
            Some((
                config.companion_memory.owner.gate(),
                config.data_dir.clone(),
            ))
        });
    let resolver: OwnerCorrectionResolver = Arc::new(|| {
        Some(OwnerCorrectionContext {
            agent_alias: "nova".into(),
            session_key: "session-a".into(),
            ingress: UserMessageIngress {
                source: UserMessageSource::Operator,
                text: "Keep replies brief.".into(),
            },
        })
    });
    let result = reiclaw_api::TOOL_LOOP_SESSION_KEY.scope(Some("session-a".into()),
        OWNER_CORRECTION_CONTEXT.scope(resolver, tool.execute(serde_json::json!({
            "kind":"preference", "statement":"Brief replies.", "semantic_key":"replies.length"
        })))).await.unwrap();
    assert!(result.success, "{:?}", result.error);
    let user = UserModelStore::shared(dir.path()).unwrap();
    assert!(user.active_heads(None).unwrap().is_empty());
    let (_server, address) = server(state).await;
    let (status, inbox) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(status, StatusCode::OK);
    let item = &inbox["items"][0];
    assert_eq!(item["kind"], "user_model_candidate");
    assert_eq!(item["item"]["scope"], "session:session-a");
    let evidence: serde_json::Value =
        serde_json::from_str(item["item"]["evidence"].as_str().unwrap()).unwrap();
    assert_eq!(evidence["messages"][0]["session_id"], "session-a");
    assert_eq!(evidence["messages"][0]["owner_text"], "Keep replies brief.");
    let review = item["review_url"].as_str().unwrap();
    let accept = serde_json::json!({"action":"accept"});
    assert_eq!(
        request(
            address,
            "POST",
            review,
            Some("bridge-token"),
            Some(accept.clone())
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(user.active_heads(None).unwrap().is_empty());
    assert_eq!(
        request(address, "POST", review, OPERATOR, Some(accept))
            .await
            .0,
        StatusCode::OK
    );
    let heads = user.active_heads(None).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].scope, "session:session-a");
}

#[derive(Default)]
struct InMemoryDigestMemory {
    digests: parking_lot::Mutex<Vec<DailyDigest>>,
}

impl reiclaw_api::attribution::Attributable for InMemoryDigestMemory {
    fn role(&self) -> reiclaw_api::attribution::Role {
        reiclaw_api::attribution::Role::Memory(reiclaw_api::attribution::MemoryKind::InMemory)
    }
    fn alias(&self) -> &str {
        "in-memory-digest"
    }
}

#[async_trait::async_trait]
impl reiclaw_api::memory_traits::Memory for InMemoryDigestMemory {
    fn name(&self) -> &str {
        "memorycore"
    }
    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: reiclaw_api::memory_traits::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<reiclaw_api::memory_traits::MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn get(
        &self,
        _key: &str,
    ) -> anyhow::Result<Option<reiclaw_api::memory_traits::MemoryEntry>> {
        Ok(None)
    }
    async fn list(
        &self,
        _category: Option<&reiclaw_api::memory_traits::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<reiclaw_api::memory_traits::MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn health_check(&self) -> bool {
        true
    }
    async fn store_with_agent(
        &self,
        _key: &str,
        _content: &str,
        _category: reiclaw_api::memory_traits::MemoryCategory,
        _session_id: Option<&str>,
        _namespace: Option<&str>,
        _importance: Option<f64>,
        _agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn recall_for_agents(
        &self,
        _allowed_agent_ids: &[&str],
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<reiclaw_api::memory_traits::MemoryEntry>> {
        Ok(Vec::new())
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

#[tokio::test]
async fn daily_digest_and_action_candidate_routes_are_operator_gated_and_roundtrip() {
    use reiclaw_api::memory_traits::Memory as _;
    let (_dir, mut state) = fixture();
    let mem = Arc::new(InMemoryDigestMemory::default());
    let mut decisions = std::collections::BTreeMap::new();
    decisions.insert(
        "Schedule weekly backup".to_string(),
        ActionCandidateStatus::Pending,
    );
    decisions.insert(
        "Archive old logs".to_string(),
        ActionCandidateStatus::Pending,
    );
    mem.publish_daily_digest(&DailyDigest {
        day_key: "2026-11-18".into(),
        agent_id: "nova".into(),
        window_start: "2026-11-17T04:00:00Z".into(),
        window_end: "2026-11-18T04:00:00Z".into(),
        narrative: "Initial machine narrative".into(),
        settled_decisions: vec!["Use MemoryCore".into()],
        why_and_tradeoffs: vec!["Single SQLite store avoids drift".into()],
        latent_insights: vec!["Backups can run weekly".into()],
        action_candidates: vec!["Schedule weekly backup".into(), "Archive old logs".into()],
        entities: vec!["zeroclaw".into()],
        created_at: "2026-11-18T04:05:00Z".into(),
        owner_edited: false,
        action_decisions: decisions,
    })
    .await
    .unwrap();
    state.mem = mem.clone();

    let (_server, address) = server(state).await;

    // Operator auth gate on write endpoints.
    for token in [None, Some("node-token"), Some("bridge-token")] {
        assert_eq!(
            request(
                address,
                "PUT",
                "/api/review/daily-digest/nova/2026-11-18",
                token,
                Some(serde_json::json!({"narrative": "hijack"})),
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(
                address,
                "POST",
                "/api/review/action-candidates/nova/2026-11-18/resolve",
                token,
                Some(serde_json::json!({"candidate": "Schedule weekly backup", "decision": "approved"})),
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }

    // Inbox returns 2 pending sleep_action_candidate items + 1 daily_digest item.
    let (status, inbox) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(status, StatusCode::OK);
    let items = inbox["items"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i["kind"] == "sleep_action_candidate")
            .count(),
        2
    );
    let digest_item = items.iter().find(|i| i["kind"] == "daily_digest").unwrap();
    assert_eq!(digest_item["item"]["owner_edited"], false);
    assert_eq!(
        digest_item["edit_url"],
        "/api/review/daily-digest/nova/2026-11-18"
    );

    // Owner edits DailyDigest -> sets owner_edited = true.
    let (status, updated) = request(
        address,
        "PUT",
        "/api/review/daily-digest/nova/2026-11-18",
        OPERATOR,
        Some(serde_json::json!({
            "narrative": "Owner refined narrative",
            "settled_decisions": ["Use MemoryCore", "Keep single DB"],
            "why_and_tradeoffs": ["Single SQLite store avoids drift"],
            "latent_insights": ["Weekly backup cadence confirmed"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["digest"]["owner_edited"], true);
    assert_eq!(updated["digest"]["narrative"], "Owner refined narrative");

    // Resolve both action candidates (1 approved, 1 dismissed).
    let (status, resolved1) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/resolve",
        OPERATOR,
        Some(serde_json::json!({
            "candidate": "Schedule weekly backup",
            "decision": "approved",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolved1["status"], "approved");

    let (status, resolved2) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/resolve",
        OPERATOR,
        Some(serde_json::json!({
            "candidate": "Archive old logs",
            "decision": "dismissed",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolved2["status"], "dismissed");

    // Pending action candidates are now resolved; daily_digest remains with owner_edited = true.
    let (_, after) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    let after_items = after["items"].as_array().unwrap();
    assert_eq!(
        after_items
            .iter()
            .filter(|i| i["kind"] == "sleep_action_candidate")
            .count(),
        0
    );
    let stored = mem.load_daily_digests("nova", 5).await.unwrap();
    assert!(stored[0].owner_edited);
    assert_eq!(
        stored[0]
            .action_decisions
            .get("Schedule weekly backup")
            .copied(),
        Some(ActionCandidateStatus::Approved)
    );
    assert_eq!(
        stored[0].action_decisions.get("Archive old logs").copied(),
        Some(ActionCandidateStatus::Dismissed)
    );
}
