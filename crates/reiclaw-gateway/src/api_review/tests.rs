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
    server_with_dispatch_timeout(state, DISPATCH_CONTROL_TIMEOUT).await
}
async fn server_with_dispatch_timeout(
    state: AppState,
    dispatch_timeout: std::time::Duration,
) -> (Server, SocketAddr) {
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
        .with_state(state.clone())
        .merge(dispatch_routes_with_timeout(state, dispatch_timeout));
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
    async fn publish_daily_digest_cas(
        &self,
        digest: &DailyDigest,
        expected_revision: u64,
    ) -> anyhow::Result<DailyDigestCasResult> {
        digest.validate()?;
        anyhow::ensure!(
            digest.revision == expected_revision.saturating_add(1),
            "CAS revision mismatch"
        );
        let mut guard = self.digests.lock();
        let Some(existing) = guard
            .iter_mut()
            .find(|d| d.agent_id == digest.agent_id && d.day_key == digest.day_key)
        else {
            return Ok(DailyDigestCasResult::NotFound);
        };
        if existing.revision != expected_revision {
            return Ok(DailyDigestCasResult::RevisionConflict {
                current_revision: existing.revision,
            });
        }
        *existing = digest.clone();
        Ok(DailyDigestCasResult::Updated(format!(
            "daily:{}:{}",
            digest.agent_id, digest.day_key
        )))
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
        revision: 1,
        candidate_items: vec![],
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
        assert_eq!(
            request(
                address,
                "POST",
                "/api/review/action-candidates/nova/2026-11-18/dispatch",
                token,
                Some(serde_json::json!({
                    "candidate_id": "cand_missing",
                    "expected_revision": 1,
                    "request_id": "req-unauth",
                    "harness": "codex"
                })),
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

#[derive(Default)]
struct FakeTachiMcp {
    calls: parking_lot::Mutex<Vec<serde_json::Value>>,
    hold_start: std::sync::atomic::AtomicBool,
    malformed_start: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl FakeTachiMcp {
    fn count(&self, action: &str) -> usize {
        self.calls
            .lock()
            .iter()
            .filter(|call| call["action"] == action)
            .count()
    }
}

async fn fake_tachi_mcp(
    State(fake): State<Arc<FakeTachiMcp>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    let id = body["id"].clone();
    match body["method"].as_str() {
        Some("initialize") => axum::Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "owned-review-tachi", "version": "0"}
            }
        }))
        .into_response(),
        Some("notifications/initialized") => StatusCode::ACCEPTED.into_response(),
        Some("tools/call") => {
            let tool = body["params"]["name"].as_str().unwrap();
            if tool != "tachi_staff" {
                return axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{"type": "text", "text": "synthetic mirror unavailable"}]
                    }
                }))
                .into_response();
            }
            assert_eq!(headers["x-tachi-agent-identity"], "reiclaw:nova");
            let args = body["params"]["arguments"].clone();
            fake.calls.lock().push(args.clone());
            let result = match args["action"].as_str().unwrap() {
                "start" => {
                    assert_eq!(args["profile"], "owner-admitted-profile");
                    assert_eq!(args["staffing_reason"], "explicit_user_request");
                    assert!(args.get("command").is_none());
                    assert!(args.get("cwd").is_none());
                    if fake.hold_start.load(std::sync::atomic::Ordering::SeqCst) {
                        fake.entered.notify_one();
                        fake.release.notified().await;
                    }
                    if fake
                        .malformed_start
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        serde_json::json!({"missing_canonical_receipt": true})
                    } else {
                        serde_json::json!({
                            "dispatch_id": "dispatch-review-1",
                            "state": "TASK_STATE_WORKING",
                            "run_dir": "/synthetic/review-run"
                        })
                    }
                }
                action => panic!("unexpected fake tachi action '{action}'"),
            };
            axum::Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "isError": false,
                    "content": [{"type": "text", "text": result.to_string()}]
                }
            }))
            .into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn start_fake_tachi() -> (Server, SocketAddr, Arc<FakeTachiMcp>) {
    let fake = Arc::new(FakeTachiMcp::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/mcp", post(fake_tachi_mcp))
        .with_state(fake.clone());
    let handle = reiclaw_log::spawn!(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (Server(handle), addr, fake)
}

fn configure_nova_for_dispatch(state: &AppState, tachi_addr: Option<SocketAddr>) {
    let mut cfg = state.config.write();
    cfg.risk_profiles.insert(
        "nova-risk".into(),
        reiclaw_config::schema::RiskProfileConfig {
            level: reiclaw_config::autonomy::AutonomyLevel::Full,
            allowed_tools: Some(vec!["tachi_start".into()]),
            ..Default::default()
        },
    );
    cfg.runtime_profiles.insert(
        "nova-runtime".into(),
        reiclaw_config::schema::RuntimeProfileConfig {
            max_actions_per_hour: 100,
            ..Default::default()
        },
    );
    if let Some(nova) = cfg.agents.get_mut("nova") {
        nova.enabled = true;
        nova.risk_profile = "nova-risk".into();
        nova.runtime_profile = "nova-runtime".into();
    }
    if let Some(addr) = tachi_addr {
        cfg.tachi.enabled = true;
        cfg.tachi.endpoint = format!("http://{addr}/mcp");
        cfg.tachi.harnesses =
            std::collections::HashMap::from([("codex".into(), "owner-admitted-profile".into())]);
    }
}

fn sample_digest(candidates: Vec<&str>) -> DailyDigest {
    DailyDigest {
        day_key: "2026-11-18".into(),
        agent_id: "nova".into(),
        window_start: "2026-11-17T04:00:00Z".into(),
        window_end: "2026-11-18T04:00:00Z".into(),
        narrative: "Review narrative for promotion tests".into(),
        settled_decisions: vec!["Keep sessions.db authoritative".into()],
        why_and_tradeoffs: vec![],
        latent_insights: vec![],
        action_candidates: candidates.into_iter().map(str::to_string).collect(),
        entities: vec!["reiclaw".into()],
        created_at: "2026-11-18T04:05:00Z".into(),
        owner_edited: false,
        action_decisions: std::collections::BTreeMap::new(),
        revision: 1,
        candidate_items: vec![],
    }
}

#[tokio::test]
async fn action_candidate_resolve_handles_ambiguous_text_cas_conflicts_and_never_starts_work() {
    use reiclaw_api::memory_traits::Memory as _;
    let (_dir, mut state) = fixture();
    let (_tachi_srv, tachi_addr, fake) = start_fake_tachi().await;
    configure_nova_for_dispatch(&state, Some(tachi_addr));

    let mem = Arc::new(InMemoryDigestMemory::default());
    let mut digest = sample_digest(vec!["Run backup", "Run backup"]);
    digest.ensure_candidate_items();
    let cand0_id = digest.candidate_items[0].id.clone();
    let cand1_id = digest.candidate_items[1].id.clone();
    assert_ne!(cand0_id, cand1_id);
    mem.publish_daily_digest(&digest).await.unwrap();
    state.mem = mem.clone();

    let (_server, address) = server(state).await;

    // Duplicate text cannot be resolved without candidate_id.
    let (status, ambig) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/resolve",
        OPERATOR,
        Some(serde_json::json!({
            "candidate": "Run backup",
            "decision": "approved"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(ambig["code"], "ambiguous_candidate");

    // Stale expected_revision returns 409 revision_conflict.
    let (status, conflict) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/resolve",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand0_id,
            "decision": "approved",
            "expected_revision": 99
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "revision_conflict");
    assert_eq!(conflict["current_revision"], 1);

    // Resolving by exact candidate_id + expected_revision succeeds and bumps revision,
    // without triggering any Tachi start.
    let (status, ok_resp) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/resolve",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand0_id,
            "decision": "approved",
            "expected_revision": 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ok_resp["revision"], 2);
    assert_eq!(ok_resp["candidate_id"], cand0_id);
    assert_eq!(ok_resp["status"], "approved");
    assert_eq!(fake.count("start"), 0);

    // Second duplicate remains pending in inbox while first is approved.
    let (_, inbox) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    let cands: Vec<_> = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "sleep_action_candidate")
        .collect();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0]["item"]["candidate_id"], cand1_id);
}

#[tokio::test]
async fn candidate_dispatch_races_different_requests_to_one_start_and_replays_across_edits() {
    let (dir, mut state) = fixture();
    let (_tachi_srv, tachi_addr, fake) = start_fake_tachi().await;
    fake.hold_start
        .store(true, std::sync::atomic::Ordering::SeqCst);
    configure_nova_for_dispatch(&state, Some(tachi_addr));

    #[cfg(feature = "memory-core")]
    let mem: Arc<dyn reiclaw_api::memory_traits::Memory> =
        Arc::new(reiclaw_memory::MemoryCore::new("nova", dir.path()).unwrap());
    #[cfg(not(feature = "memory-core"))]
    let mem: Arc<dyn reiclaw_api::memory_traits::Memory> =
        Arc::new(InMemoryDigestMemory::default());
    let _ = &dir;

    let mut digest = sample_digest(vec!["Audit workspace dependencies"]);
    digest.ensure_candidate_items();
    let cand_id = digest.candidate_items[0].id.clone();
    mem.publish_daily_digest(&digest).await.unwrap();
    state.mem = mem.clone();

    let (_server, address) = server(state).await;

    let body_a = serde_json::json!({
        "candidate_id": cand_id,
        "expected_revision": 1,
        "request_id": "req-race-a",
        "harness": "codex",
        "context_brief": "Compare workspace crates"
    });
    let body_b = serde_json::json!({
        "candidate_id": cand_id,
        "expected_revision": 1,
        "request_id": "req-race-b",
        "harness": "codex",
        "context_brief": "Compare workspace crates"
    });

    let fut_a = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(body_a.clone()),
    );
    let fut_b = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(body_b.clone()),
    );
    let releaser = async {
        fake.entered.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        fake.release.notify_waiters();
    };
    let (res_a, res_b, ()) = tokio::join!(fut_a, fut_b, releaser);

    assert_eq!(fake.count("start"), 1);
    let responses = [res_a, res_b];
    let ok_res = responses
        .iter()
        .find(|(s, _)| *s == StatusCode::OK)
        .unwrap_or_else(|| panic!("one racing dispatch must succeed: {responses:?}"));
    let conflict_res = responses
        .iter()
        .find(|(s, _)| *s == StatusCode::CONFLICT)
        .unwrap_or_else(|| panic!("losing racing dispatch must conflict: {responses:?}"));
    assert_eq!(ok_res.1["accepted"], true);
    assert_eq!(ok_res.1["dispatch_id"], "dispatch-review-1");
    assert_eq!(ok_res.1["state"], "bound");
    assert_eq!(conflict_res.1["code"], "candidate_already_dispatched");

    // Display-text edit via PUT /daily-digest bumps revision 1 -> 2, preserves candidate_id
    // and keeps action_dispatches projection intact in GET /api/review/inbox.
    let (status, edited) = request(
        address,
        "PUT",
        "/api/review/daily-digest/nova/2026-11-18",
        OPERATOR,
        Some(serde_json::json!({
            "narrative": "Updated narrative after promotion",
            "expected_revision": 1,
            "candidate_edits": [
                {"candidate_id": cand_id, "text": "Audit Cargo.lock and workspace dependencies"}
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["digest"]["revision"], 2);
    assert_eq!(edited["digest"]["candidate_items"][0]["id"], cand_id);
    assert_eq!(
        edited["digest"]["action_dispatches"][&cand_id]["dispatch_id"],
        "dispatch-review-1"
    );

    // Unchanged same-ID replay (passing explicit original task) succeeds even after revision edit.
    let mut replay_body = if ok_res.1["request_id"] == "req-race-a" {
        body_a
    } else {
        body_b
    };
    replay_body["task"] = serde_json::json!("Audit workspace dependencies");
    let (status, replayed) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(replay_body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["dispatch_id"], "dispatch-review-1");
    assert_eq!(fake.count("start"), 1);

    // Modified arguments with the same request_id fail with 409 request_conflict.
    let mut modified_body = replay_body;
    modified_body["task"] = serde_json::json!("Do something else");
    let (status, req_conflict) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(modified_body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(req_conflict["code"], "request_conflict");
    assert_eq!(fake.count("start"), 1);

    // Inbox projection reflects dispatch on both sleep_action_candidate and daily_digest.
    let (status, inbox) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(status, StatusCode::OK);
    let cand_item = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "sleep_action_candidate")
        .unwrap();
    assert_eq!(cand_item["item"]["candidate_id"], cand_id);
    assert_eq!(cand_item["item"]["source_revision"], 2);
    assert_eq!(
        cand_item["item"]["dispatch"]["dispatch_id"],
        "dispatch-review-1"
    );
}

#[tokio::test]
async fn candidate_dispatch_preserves_unknown_claims_on_timeout_and_malformed_receipt_and_heals() {
    use reiclaw_api::memory_traits::Memory as _;
    let (dir, mut state) = fixture();
    let (_tachi_srv, tachi_addr, fake) = start_fake_tachi().await;
    fake.hold_start
        .store(true, std::sync::atomic::Ordering::SeqCst);
    configure_nova_for_dispatch(&state, Some(tachi_addr));

    let mem = Arc::new(InMemoryDigestMemory::default());
    let mut digest = sample_digest(vec!["Timeout candidate", "Malformed receipt candidate"]);
    digest.ensure_candidate_items();
    let cand_timeout = digest.candidate_items[0].id.clone();
    let cand_malformed = digest.candidate_items[1].id.clone();
    mem.publish_daily_digest(&digest).await.unwrap();
    state.mem = mem;

    // Ensure initial DB exists so cold schema init does not eat the timeout budget.
    SqliteSessionBackend::new(dir.path()).unwrap();

    let (_server, address) =
        server_with_dispatch_timeout(state.clone(), std::time::Duration::from_millis(300)).await;

    // 1. Timeout marks claim SubmissionUnknown; neither different nor same ID starts again.
    let timeout_req = serde_json::json!({
        "candidate_id": cand_timeout,
        "expected_revision": 1,
        "request_id": "req-timeout-1",
        "harness": "codex"
    });
    let ((status, timeout_resp), ()) = tokio::join!(
        request(
            address,
            "POST",
            "/api/review/action-candidates/nova/2026-11-18/dispatch",
            OPERATOR,
            Some(timeout_req.clone()),
        ),
        fake.entered.notified(),
    );
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(timeout_resp["code"], "submission_unknown");

    fake.hold_start
        .store(false, std::sync::atomic::Ordering::SeqCst);
    fake.release.notify_waiters();
    tokio::task::yield_now().await;

    // Different request_id must be rejected with candidate_already_dispatched.
    let (status, second) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_timeout,
            "expected_revision": 1,
            "request_id": "req-timeout-2",
            "harness": "codex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(second["code"], "candidate_already_dispatched");

    // 2. Malformed receipt from Tachi leaves SubmissionUnknown.
    let calls_before = fake.count("start");
    fake.malformed_start
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let malformed_req = serde_json::json!({
        "candidate_id": cand_malformed,
        "expected_revision": 1,
        "request_id": "req-malformed-1",
        "harness": "codex"
    });
    let (status, mal_resp) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(malformed_req.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(mal_resp["code"], "submission_unknown");
    assert_eq!(fake.count("start"), calls_before + 1);

    // Same request_id replay while still unknown stays blocked (submission_unresolved).
    let (status, unres) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(malformed_req.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(unres["code"], "submission_unresolved");
    assert_eq!(fake.count("start"), calls_before + 1);

    // 3. Simulate crash recovery where session_delegations recorded dispatch_id
    // while candidate row remained SubmissionUnknown: inbox & replay self-heal to Bound.
    let route = route_fingerprint(&state.config.read().tachi, "nova");
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    let existing = db
        .read_delegation_request("nova", "req-malformed-1")
        .unwrap()
        .unwrap();
    db.bind_delegation_request(
        "nova",
        "req-malformed-1",
        &existing.request_digest,
        &route,
        "dispatch-healed-9",
    )
    .unwrap();

    let (status, inbox) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(status, StatusCode::OK);
    let healed_digest = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "daily_digest")
        .unwrap();
    assert_eq!(
        healed_digest["item"]["action_dispatches"][&cand_malformed]["state"],
        "bound"
    );
    assert_eq!(
        healed_digest["item"]["action_dispatches"][&cand_malformed]["dispatch_id"],
        "dispatch-healed-9"
    );

    let (status, healed_replay) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(malformed_req),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(healed_replay["replayed"], true);
    assert_eq!(healed_replay["dispatch_id"], "dispatch-healed-9");
    assert_eq!(fake.count("start"), calls_before + 1);
}

#[tokio::test]
async fn candidate_dispatch_enforces_markers_dismissal_estop_and_unsent_release() {
    use reiclaw_api::memory_traits::Memory as _;
    let (dir, mut state) = fixture();
    let (_tachi_srv, tachi_addr, fake) = start_fake_tachi().await;

    // Bind and immediately close a listener so first attempt fails as provably unsent Unavailable.
    let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead_listener.local_addr().unwrap();
    drop(dead_listener);

    configure_nova_for_dispatch(&state, Some(dead_addr));
    let mem = Arc::new(InMemoryDigestMemory::default());
    let mut digest = sample_digest(vec!["Check release readiness", "Skip cleanup"]);
    digest.ensure_candidate_items();
    let cand_ok = digest.candidate_items[0].id.clone();
    let cand_dismiss = digest.candidate_items[1].id.clone();
    mem.publish_daily_digest(&digest).await.unwrap();
    state.mem = mem;

    let (_server, address) = server(state.clone()).await;

    // Protected markers, admission-forbidden payloads, oversized brief, unknown harness create zero claims.
    for (label, body, expected_code) in [
        (
            "soul marker in brief",
            serde_json::json!({
                "candidate_id": cand_ok, "expected_revision": 1, "request_id": "req-m1",
                "harness": "codex", "context_brief": "soul::nova identity"
            }),
            "forbidden_content",
        ),
        (
            "command text in task",
            serde_json::json!({
                "candidate_id": cand_ok, "expected_revision": 1, "request_id": "req-m2",
                "harness": "codex", "task": "bash -c whoami"
            }),
            "forbidden_content",
        ),
        (
            "oversized brief",
            serde_json::json!({
                "candidate_id": cand_ok, "expected_revision": 1, "request_id": "req-m3",
                "harness": "codex", "context_brief": "x".repeat(4097)
            }),
            "invalid_arguments",
        ),
        (
            "unknown harness",
            serde_json::json!({
                "candidate_id": cand_ok, "expected_revision": 1, "request_id": "req-m4",
                "harness": "unlisted-harness"
            }),
            "unknown_harness",
        ),
    ] {
        let (status, resp) = request(
            address,
            "POST",
            "/api/review/action-candidates/nova/2026-11-18/dispatch",
            OPERATOR,
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {resp}");
        assert_eq!(resp["code"], expected_code, "{label}: {resp}");
    }

    // Dismissing a candidate prevents dispatch (409 candidate_dismissed).
    let (status, _) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/resolve",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_dismiss,
            "decision": "dismissed",
            "expected_revision": 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, dismissed_resp) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_dismiss,
            "expected_revision": 2,
            "request_id": "req-dismissed",
            "harness": "codex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(dismissed_resp["code"], "candidate_dismissed");

    // Provably unsent Unavailable releases claim so retry against live Tachi succeeds.
    let (status, unavail) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_ok,
            "expected_revision": 2,
            "request_id": "req-retry-after-unavail",
            "harness": "codex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unavail["code"], "unavailable");

    // Estop blocking tachi_start refuses pre-claim with 403 denied.
    let estop_path = dir.path().join("estop-state.json");
    std::fs::write(
        &estop_path,
        serde_json::to_vec(&reiclaw_policy::estop::EstopState {
            frozen_tools: vec!["tachi_start".into()],
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    {
        let mut cfg = state.config.write();
        cfg.tachi.endpoint = format!("http://{tachi_addr}/mcp");
        cfg.security.estop.enabled = true;
        cfg.security.estop.state_file = estop_path.to_string_lossy().into_owned();
    }
    let (status, estop_resp) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_ok,
            "expected_revision": 2,
            "request_id": "req-retry-after-unavail",
            "harness": "codex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(estop_resp["code"], "denied");

    // Disable estop and retry with same candidate: released prior claim allows success now.
    state.config.write().security.estop.enabled = false;
    let (status, ok_resp) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_ok,
            "expected_revision": 2,
            "request_id": "req-retry-after-unavail",
            "harness": "codex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ok_resp["accepted"], true);
    assert_eq!(ok_resp["dispatch_id"], "dispatch-review-1");
    assert_eq!(fake.count("start"), 1);
}

#[tokio::test]
async fn candidate_edits_do_not_leak_stale_text_decision_onto_pending_candidate() {
    use reiclaw_api::memory_traits::Memory as _;
    let (_dir, mut state) = fixture();
    let (_tachi_srv, tachi_addr, fake) = start_fake_tachi().await;
    configure_nova_for_dispatch(&state, Some(tachi_addr));

    let mem = Arc::new(InMemoryDigestMemory::default());
    let mut digest = sample_digest(vec!["Check X", "Check Y"]);
    digest.ensure_candidate_items();
    let cand_dismissed = digest.candidate_items[0].id.clone();
    let cand_pending = digest.candidate_items[1].id.clone();
    mem.publish_daily_digest(&digest).await.unwrap();
    state.mem = mem.clone();

    let (_server, address) = server(state).await;

    let (status, _) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/resolve",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_dismissed,
            "decision": "dismissed",
            "expected_revision": 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = request(
        address,
        "PUT",
        "/api/review/daily-digest/nova/2026-11-18",
        OPERATOR,
        Some(serde_json::json!({
            "narrative": "Reworded digest",
            "expected_revision": 2,
            "candidate_edits": [
                { "candidate_id": cand_dismissed, "text": "Check Z" },
                { "candidate_id": cand_pending, "text": "Check X" }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let stored = mem
        .load_daily_digests("nova", 8)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.day_key == "2026-11-18")
        .unwrap();
    let dismissed_item = stored
        .candidate_items
        .iter()
        .find(|c| c.id == cand_dismissed)
        .unwrap();
    let pending_item = stored
        .candidate_items
        .iter()
        .find(|c| c.id == cand_pending)
        .unwrap();
    assert_eq!(
        stored.candidate_status(dismissed_item),
        reiclaw_api::sleep_records::ActionCandidateStatus::Dismissed
    );
    assert_eq!(
        stored.candidate_status(pending_item),
        reiclaw_api::sleep_records::ActionCandidateStatus::Pending
    );
    assert!(!stored.action_decisions.contains_key("Check X"));

    let (status, blocked) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_dismissed,
            "expected_revision": 3,
            "request_id": "req-still-dismissed",
            "harness": "codex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(blocked["code"], "candidate_dismissed");

    let (status, dispatched) = request(
        address,
        "POST",
        "/api/review/action-candidates/nova/2026-11-18/dispatch",
        OPERATOR,
        Some(serde_json::json!({
            "candidate_id": cand_pending,
            "expected_revision": 3,
            "request_id": "req-still-pending",
            "harness": "codex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected error: {dispatched}");
    assert_eq!(dispatched["accepted"], true);
    assert_eq!(fake.count("start"), 1);
}
