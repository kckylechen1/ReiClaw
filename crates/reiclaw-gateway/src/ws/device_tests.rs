use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use reiclaw_config::schema::{
    AliasedAgentConfig, OpenAIModelProviderConfig, GatewayBridgeConfig, GatewayDeviceBridgeConfig,
    ModelProviderConfig, RiskProfileConfig, RuntimeProfileConfig,
};

const DEVICE_TOKEN: &str = "synthetic-device-bridge-token";
const OWNER_TOKEN: &str = "synthetic-paired-owner-token";
const SESSION: &str = "device-session";
const LIMIT: Duration = Duration::from_secs(15);

/// Real transport client for adversarial frames that the typed client cannot express.
/// All I/O is bounded; credentials are synthetic and endpoints are loopback-only.
pub(super) struct DeviceWire(tokio::net::TcpStream);

impl DeviceWire {
    pub(super) async fn upgrade(address: std::net::SocketAddr, query: &str, token: &str) -> (u16, Self) {
        Self::upgrade_path(address, &format!("/ws/chat?{query}"), token).await
    }

    async fn upgrade_path(address: std::net::SocketAddr, path: &str, token: &str) -> (u16, Self) {
        let mut stream = tokio::time::timeout(LIMIT, tokio::net::TcpStream::connect(address))
            .await
            .unwrap()
            .unwrap();
        let protocol = if path.starts_with("/ws/bridge") {
            "zeroclaw.bridge.v2"
        } else {
            "reiclaw.v1"
        };
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}\r\nAuthorization: Bearer {token}\r\n\r\n"
        );
        tokio::time::timeout(LIMIT, stream.write_all(request.as_bytes()))
            .await
            .unwrap()
            .unwrap();
        let mut header = Vec::new();
        tokio::time::timeout(LIMIT, async {
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 16_384, "oversized upgrade response");
                header.push(stream.read_u8().await.unwrap());
            }
        })
        .await
        .unwrap();
        let status = std::str::from_utf8(&header)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, Self(stream))
    }

    pub(super) async fn send_opcode(&mut self, opcode: u8, payload: &[u8]) {
        assert!(payload.len() < 65_536);
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        // RFC6455 masking matters to the real Axum/tungstenite server, not secrecy.
        let mask = [0x12, 0x34, 0x56, 0x78];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        tokio::time::timeout(LIMIT, self.0.write_all(&frame))
            .await
            .unwrap()
            .unwrap();
    }

    pub(super) async fn send(&mut self, value: Value) {
        self.send_opcode(1, value.to_string().as_bytes()).await;
    }

    pub(super) async fn next(&mut self) -> Option<Value> {
        self.next_with_limit(65_536).await
    }

    async fn next_with_limit(&mut self, max_bytes: u64) -> Option<Value> {
        tokio::time::timeout(LIMIT, async {
            loop {
                let first = match self.0.read_u8().await {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return None,
                    Err(error) => panic!("WebSocket read failed: {error}"),
                };
                let second = self.0.read_u8().await.unwrap();
                assert_eq!(first & 0x80, 0x80, "test gateway frames must not fragment");
                assert_eq!(second & 0x80, 0, "server must not mask frames");
                let len = match second & 0x7f {
                    126 => u64::from(self.0.read_u16().await.unwrap()),
                    127 => self.0.read_u64().await.unwrap(),
                    len => u64::from(len),
                };
                assert!(len <= max_bytes, "oversized test frame");
                let mut payload = vec![0; len as usize];
                self.0.read_exact(&mut payload).await.unwrap();
                match first & 0xf {
                    1 => return Some(serde_json::from_slice(&payload).unwrap()),
                    8 => return None,
                    9 => self.send_opcode(10, &payload).await,
                    10 => {}
                    other => panic!("unexpected WebSocket opcode {other}"),
                }
            }
        })
        .await
        .expect("bounded WebSocket response")
    }

    pub(super) async fn until(&mut self, kind: &str) -> Value {
        for _ in 0..64 {
            let frame = self
                .next()
                .await
                .expect("gateway closed before expected frame");
            if frame["type"] == kind {
                return frame;
            }
            assert_ne!(frame["type"], "error", "unexpected gateway error: {frame}");
        }
        panic!("too many frames before {kind}")
    }

    async fn connected(&mut self, connect: Value) {
        assert_eq!(self.until("session_start").await["session_id"], SESSION);
        self.send(connect).await;
        self.until("connected").await;
    }

    async fn denied(&mut self) {
        // Immediate close is also a valid live revocation contract.
        if let Some(frame) = self.next().await {
            assert_eq!(frame["code"], "BRIDGE_OPERATION_DENIED", "{frame}");
        }
    }
}

#[cfg(feature = "voice-surface")]
mod voice_projection_tests {
    include!("voice_projection_tests.rs");
}

/// A deterministic local OpenAI-compatible provider. The gateway still resolves
/// config and constructs its real Agent, tool registry, approval channel and
/// persisted session. No Conversation or provider trait object is injected.
async fn device_provider(
    State(seen): State<Arc<parking_lot::Mutex<Vec<Value>>>>,
    axum::Json(request): axum::Json<Value>,
) -> impl IntoResponse {
    seen.lock().push(request.clone());
    if let Some(response) = turn_control_tests::provider_response(&request).await {
        return response;
    }
    let input = request["input"].as_array().unwrap();
    let user_contains = |item: &Value, needle: &str| {
        item["role"] == "user" && item["content"].to_string().contains(needle)
    };
    let tool_needed = input.iter().any(|item| user_contains(item, "approval-probe"))
        && !input.iter().any(|item| item["type"] == "function_call_output");
    let confinement_probe = input.last().is_some_and(|item| user_contains(item, "device-tool-probe"));
    let cap_probe = input.iter().any(|item| user_contains(item, "device-cap-probe"));
    let response = if cap_probe {
        json!({"output":[{"type":"function_call","call_id":format!("cap-{}", input.len()),
            "name":format!("absent_{}", input.len()),"arguments":"{}"}]})
    } else if confinement_probe {
        let tools = ["shell", "file_write", "notify", "memory_store", "sessions_send", "cron_add", "public__probe"];
        let calls = tools.iter().enumerate().map(|(index, name)| json!({
            "type":"function_call","call_id":format!("device-probe-{index}"),"name":name,
            "arguments":json!({"path":"forbidden.txt","content":"forbidden","command":"touch forbidden.txt"}).to_string(),
        })).collect::<Vec<_>>();
        json!({"output":calls})
    } else if tool_needed {
        json!({"output":[{"type":"function_call","call_id":"approval-file-call","name":"file_write",
            "arguments":json!({"path":"approved.txt","content":"owner-approved"}).to_string()}]})
    } else {
        json!({"output_text":"device response"})
    };
    if request["stream"] == true {
        let completed = json!({"type":"response.completed","response":response});
        ([("content-type", "text/event-stream")], format!("data: {completed}\n\n")).into_response()
    } else {
        axum::Json(response).into_response()
    }
}

struct DeviceFixture {
    _tmp: tempfile::TempDir,
    state: AppState,
    address: std::net::SocketAddr,
    seen: Arc<parking_lot::Mutex<Vec<Value>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    workspace: PathBuf,
}

impl Drop for DeviceFixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl DeviceFixture {
    async fn new(operations: &[&str]) -> Self {
        Self::with_pairing(operations, true).await
    }

    async fn with_pairing(operations: &[&str], require_pairing: bool) -> Self {
        Self::with_address(operations, require_pairing, "127.0.0.1:0".parse().unwrap()).await
    }

    async fn with_address(operations: &[&str], require_pairing: bool, address: std::net::SocketAddr) -> Self {
        assert!(address.ip().is_loopback());
        let tmp = tempfile::tempdir().unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_uri = format!(
            "http://{}/v1/responses",
            provider_listener.local_addr().unwrap()
        );
        let provider_app = axum::Router::new()
            .route("/v1/responses", axum::routing::post(device_provider))
            .with_state(Arc::clone(&seen));
        let provider_task = ::reiclaw_log::spawn!(async move {
            axum::serve(provider_listener, provider_app).await.unwrap();
        });
        let mut state = crate::tests::admin_paircode_state(&tmp, require_pairing, false);
        // A collision must not turn a restricted bridge into a paired Operator.
        state.pairing = Arc::new(reiclaw_runtime::security::pairing::PairingGuard::new(
            require_pairing,
            &[OWNER_TOKEN.into(), DEVICE_TOKEN.into()],
        ));
        state.auto_save = true;
        state.session_backend = Some(Arc::new(
            reiclaw_infra::session_sqlite::SqliteSessionBackend::new(tmp.path()).unwrap(),
        ));
        let workspace = tmp.path().join("agent-workspace");
        std::fs::create_dir(&workspace).unwrap();
        {
            let mut config = state.config.write();
            config.providers.models.openai.insert(
                "test".into(),
                OpenAIModelProviderConfig {
                    base: ModelProviderConfig {
                        model: Some("local-device-test".into()),
                        uri: Some(provider_uri),
                        timeout_secs: Some(5),
                        ..Default::default()
                    },
                },
            );
            config.risk_profiles.insert(
                "test".into(),
                RiskProfileConfig {
                    workspace_only: true,
                    allowed_tools: Some(vec!["file_write".into()]),
                    always_ask: vec!["file_write".into()],
                    auto_approve: Vec::new(),
                    ..Default::default()
                },
            );
            config.risk_profiles.insert("public-device".into(), RiskProfileConfig {
                level: reiclaw_config::autonomy::AutonomyLevel::Supervised,
                allowed_tools: Some(vec![]), auto_approve: vec![], always_ask: vec![],
                mcp_discovered_tool_policy: reiclaw_config::autonomy::McpDiscoveredToolPolicy::ExplicitOnly,
                ..Default::default()
            });
            config.runtime_profiles.insert(
                "test".into(),
                RuntimeProfileConfig {
                    agentic: true,
                    max_tool_iterations: 4,
                    ..Default::default()
                },
            );
            for alias in ["device-agent", "other-agent"] {
                let mut agent = AliasedAgentConfig {
                    model_provider: "openai.test".into(),
                    risk_profile: "test".into(),
                    runtime_profile: "test".into(),
                    ..Default::default()
                };
                agent.workspace.path = Some(workspace.clone());
                agent.memory.backend = reiclaw_config::multi_agent::MemoryBackendKind::None;
                config.agents.insert(alias.into(), agent);
            }
            config.gateway.bridges.insert(
                "device".into(),
                GatewayBridgeConfig {
                    token_hash: reiclaw_config::tokens::hash_token(DEVICE_TOKEN),
                    sessions: vec![SESSION.into()],
                    session_prefix: None,
                    owner: None,
                    device: Some(GatewayDeviceBridgeConfig {
                        risk_profile: Some("public-device".into()),
                        agent: "device-agent".into(),
                        operations: operations.iter().map(|op| (*op).into()).collect(),
                    }),
                },
            );
            config.gateway.bridges["device"].device.as_ref().unwrap()
                .validated_profile(&config).expect("valid public-tier device fixture");
        }
        let app = axum::Router::new()
            .route("/ws/chat", axum::routing::get(handle_ws_chat))
            .route(
                "/api/sessions/{id}",
                axum::routing::delete(crate::api::handle_api_session_delete),
            )
            .route(
                "/ws/bridge",
                axum::routing::get(crate::ws_bridge::handle_ws_bridge),
            )
            .with_state(state.clone())
            .merge(crate::api_attachments::routes(state.clone(), 5));
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let gateway_task = ::reiclaw_log::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _tmp: tmp,
            state,
            address,
            seen,
            tasks: vec![provider_task, gateway_task],
            workspace,
        }
    }

    async fn socket(&self, token: &str) -> DeviceWire {
        let owner_namespace = token == OWNER_TOKEN && self.state.config.read().gateway.bridges.values().any(|b| b.device.is_some());
        let session = if owner_namespace { "owner-session" } else { SESSION };
        let (status, mut client) = DeviceWire::upgrade(
            self.address,
            &format!("agent=device-agent&session_id={session}"),
            token,
        )
        .await;
        assert_eq!(status, 101);
        assert_eq!(client.until("session_start").await["session_id"], session);
        client.send(json!({"type":"connect"})).await;
        client.until("connected").await;
        client
    }

    async fn send_turn(&self, client: &mut DeviceWire, id: &str, content: &str) {
        client
            .send(json!({"type":"message", "id":id, "content":content}))
            .await;
        assert_eq!(client.until("ack").await["status"], "accepted");
        client.until("done").await;
    }
}

#[tokio::test]
async fn device_wire_binding_is_trusted_and_conflicts_fail_before_provider_use() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    for query in [
        "agent=other-agent&session_id=device-session",
        "agent=device-agent&session_id=wrong-session",
        "agent=device-agent",
        "agent=device-agent&session_id=device-session&cwd=%2Ftmp",
    ] {
        let (status, _) = DeviceWire::upgrade(fixture.address, query, DEVICE_TOKEN).await;
        assert_eq!(status, 403, "query {query}");
    }
    for extra in [
        json!({"session_id":"wrong-session"}),
        json!({"session_id":null}),
        json!({"agent":"other-agent"}),
        json!({"agent_alias":"other-agent"}),
        json!({"agentAlias":"other-agent"}),
        json!({"cwd":"/tmp"}),
        json!({"workspaceDir":"/tmp"}),
        json!({"session_id":7}),
    ] {
        let (status, mut client) = DeviceWire::upgrade(
            fixture.address,
            "agent=device-agent&session_id=device-session",
            DEVICE_TOKEN,
        )
        .await;
        assert_eq!(status, 101);
        client.until("session_start").await;
        let mut connect = json!({"type":"connect"});
        connect
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        client.send(connect).await;
        assert_eq!(
            client.until("error").await["code"],
            "BRIDGE_BINDING_REJECTED"
        );
    }
    assert!(fixture.seen.lock().is_empty());
    let (status, mut client) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session",
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    client
        .connected(json!({"type":"connect", "session_id":SESSION, "agent":"device-agent"}))
        .await;
    fixture
        .send_turn(&mut client, "allowed-chat", "hello device")
        .await;
    let seen = fixture.seen.lock();
    assert_eq!(seen.len(), 1, "one admitted turn reaches the real provider");
    assert_eq!(seen[0]["model"], "local-device-test");
    assert!(seen[0]["input"].to_string().contains("hello device"));
}

#[tokio::test]
async fn device_wire_cannot_consume_owner_approval_but_owner_can_approve() {
    let fixture = DeviceFixture::new(&["chat", "answer", "cancel"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    let mut owner = fixture.socket(OWNER_TOKEN).await;
    owner
        .send(json!({"type":"message", "id":"needs-approval", "content":"approval-probe"}))
        .await;
    let approval = owner.until("approval_request").await;
    assert_eq!(approval["tool"], "file_write");
    for decision in ["approve", "always", "deny"] {
        device.send(json!({"type":"approval_response", "request_id":approval["request_id"], "decision":decision})).await;
        assert_eq!(
            device.until("error").await["code"],
            "BRIDGE_OPERATION_DENIED"
        );
        assert!(!fixture.workspace.join("approved.txt").exists());
    }
    // Successful owner completion proves the attack did not consume the pending oneshot.
    owner.send(json!({"type":"approval_response", "request_id":approval["request_id"], "decision":"approve"})).await;
    owner.until("done").await;
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("approved.txt")).unwrap(),
        "owner-approved"
    );
    assert_eq!(fixture.seen.lock().len(), 2);
}

#[tokio::test]
async fn device_wire_each_operation_is_closed_unless_granted() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    for frame in [
        json!({"type":"cancel"}),
        json!({"type":"answer", "request_id":"missing", "text":"yes"}),
        json!({"type":"source_resume", "source":"telegram:attacker"}),
        json!({"type":"source_disposition", "source":"telegram:attacker"}),
        json!({"type":"message", "id":"source-claim", "content":"no", "source":{}}),
        json!({"type":"message", "id":"attachment-claim", "content":"no", "attachments":["bogus"]}),
        json!({"type":"future-capability"}),
    ] {
        device.send(frame).await;
        assert_eq!(
            device.until("error").await["code"],
            "BRIDGE_OPERATION_DENIED"
        );
    }
    assert!(fixture.seen.lock().is_empty());
    fixture
        .send_turn(&mut device, "still-authorized", "chat remains allowed")
        .await;
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .get_mut("device")
        .unwrap()
        .device
        .as_mut()
        .unwrap()
        .operations
        .clear();
    device
        .send(json!({"type":"message", "id":"reduced", "content":"blocked"}))
        .await;
    device.denied().await;
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn device_wire_revocation_and_policy_removal_cannot_elevate_open_socket() {
    for remove_entry in [true, false] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        // Revoke this bridge credential alone; the collision case has its own test.
        assert!(fixture.state.pairing.revoke_token(DEVICE_TOKEN));
        let mut device = fixture.socket(DEVICE_TOKEN).await;
        fixture
            .send_turn(&mut device, "before-revoke", "allowed before change")
            .await;
        {
            let mut config = fixture.state.config.write();
            if remove_entry {
                config.gateway.bridges.remove("device");
            } else {
                config.gateway.bridges.get_mut("device").unwrap().device = None;
            }
        }
        device
            .send(json!({"type":"message", "id":"after-revoke", "content":"must never run"}))
            .await;
        device.denied().await;
        assert_eq!(fixture.seen.lock().len(), 1);
        if remove_entry {
            let (status, _) = DeviceWire::upgrade(
                fixture.address,
                "agent=device-agent&session_id=device-session",
                DEVICE_TOKEN,
            )
            .await;
            assert_eq!(status, 401, "revoked bridge cannot open a fresh socket");
        }
    }
}

#[tokio::test]
async fn device_wire_omitted_operations_deny_attachment() {
    let fixture = DeviceFixture::new(&[]).await;
    let (status, _) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session",
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 403);
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn device_wire_input_never_becomes_operator_even_with_paired_token_collision() {
    use reiclaw_api::review::UserMessageSource;
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    fixture
        .send_turn(
            &mut device,
            "device-input",
            "device must not teach the operator model",
        )
        .await;
    let mut owner = fixture.socket(OWNER_TOKEN).await;
    fixture
        .send_turn(&mut owner, "owner-input", "owner authorized input")
        .await;
    let rows = fixture
        .state
        .session_backend
        .as_ref()
        .unwrap()
        .load_with_timestamps("gw_device-session");
    let device_rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.message.role == "user" && row.message.content.contains("device must not teach")
        })
        .collect();
    assert_eq!(
        device_rows.len(),
        1,
        "the real device turn must have persisted"
    );
    assert!(
        device_rows[0].ingress.is_none(),
        "device text is not Operator learning evidence"
    );
    let owner_history = fixture.state.session_backend.as_ref().unwrap().load_with_timestamps("gw_owner-session");
    let owner_rows: Vec<_> = owner_history
        .iter()
        .filter(|row| {
            row.ingress
                .as_ref()
                .is_some_and(|ingress| ingress.source == UserMessageSource::Operator)
        })
        .collect();
    assert_eq!(owner_rows.len(), 1, "owner provenance remains available");
    assert_eq!(
        owner_rows[0].ingress.as_ref().unwrap().text,
        "owner authorized input"
    );
}

#[tokio::test]
async fn device_wire_cannot_rebind_an_existing_sessions_agent() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let backend = fixture.state.session_backend.as_ref().unwrap();
    backend
        .set_session_agent_alias("gw_device-session", "other-agent")
        .unwrap();
    let (status, mut device) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session",
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    // Refuse before session_start, history disclosure or Agent construction.
    let frame = device.next().await.unwrap();
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "BRIDGE_BINDING_REJECTED");
    assert_eq!(
        backend
            .get_session_agent_alias("gw_device-session")
            .unwrap()
            .as_deref(),
        Some("other-agent")
    );
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn device_wire_empty_and_noncanonical_scoped_sessions_are_rejected() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    {
        let mut config = fixture.state.config.write();
        let bridge = config.gateway.bridges.get_mut("device").unwrap();
        bridge.sessions = vec![String::new()];
        bridge.session_prefix = Some("device/".into());
        assert!(bridge.allows_session(""));
        assert!(bridge.allows_session("device/room"));
    }
    for query in [
        "agent=device-agent&session_id=",
        "agent=device-agent&session_id=device%2Froom",
    ] {
        let (status, _) = DeviceWire::upgrade(fixture.address, query, DEVICE_TOKEN).await;
        assert_eq!(status, 403, "scoped but noncanonical session: {query}");
    }
    assert!(fixture.seen.lock().is_empty());
}

/// Real HTTP caller for the auxiliary attachment boundary; not router.oneshot.
async fn device_http(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: &str,
) -> (u16, Vec<u8>) {
    tokio::time::timeout(LIMIT, async {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.take(65_537).read_to_end(&mut response).await.unwrap();
        assert!(response.len() <= 65_536, "oversized HTTP response");
        let boundary = response.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&response[..boundary]).unwrap()
            .split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, response[boundary + 4..].to_vec())
    }).await.expect("bounded HTTP response")
}

#[tokio::test]
async fn device_wire_cannot_open_outbox_or_upload_or_fetch_attachments() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let (status, _) = DeviceWire::upgrade_path(fixture.address, "/ws/bridge", DEVICE_TOKEN).await;
    assert_eq!(
        status, 401,
        "device policy never grants the outbox transport"
    );
    let upload = "/api/attachments?agent=device-agent&session_id=device-session&file_name=test.txt";
    assert_eq!(
        device_http(fixture.address, "POST", upload, DEVICE_TOKEN, "device data")
            .await
            .0,
        403
    );
    // The same route remains usable by a paired owner, including a real stored handle.
    let (status, body) =
        device_http(fixture.address, "POST", upload, OWNER_TOKEN, "owner data").await;
    assert_eq!(status, 201);
    let attachment: Value = serde_json::from_slice(&body).unwrap();
    let fetch = format!(
        "/api/attachments/{}?agent=device-agent&session_id=device-session",
        attachment["id"].as_str().unwrap()
    );
    assert_eq!(
        device_http(fixture.address, "GET", &fetch, DEVICE_TOKEN, "")
            .await
            .0,
        403
    );
    let (status, body) = device_http(fixture.address, "GET", &fetch, OWNER_TOKEN, "").await;
    assert_eq!(status, 200);
    assert_eq!(body, b"owner data");
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn device_wire_rechecks_revocation_after_waiting_for_session_queue() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    let guard = fixture
        .state
        .session_queue
        .acquire("gw_device-session")
        .await
        .unwrap();
    device.send(json!({"type":"message", "id":"queued-before-revoke", "content":"must not reach provider"})).await;
    assert_eq!(device.until("ack").await["status"], "accepted");
    // Observe the actual run waiting behind the held permit, not a timing guess.
    tokio::time::timeout(LIMIT, async {
        while fixture
            .state
            .session_queue
            .queue_depth("gw_device-session")
            .await
            != 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(fixture.seen.lock().is_empty());
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .remove("device");
    drop(guard);
    // The socket's output guard may close before forwarding the turn's denial.
    // The persisted terminal receipt proves the queued task itself was refused.
    device.denied().await;
    tokio::time::timeout(LIMIT, async {
        loop {
            let receipt = fixture
                .state
                .session_backend
                .as_ref()
                .unwrap()
                .record_request("gw_device-session", "queued-before-revoke", "accepted")
                .unwrap()
                .expect("SQLite must retain the accepted request");
            let RequestReceipt::Duplicate { state } = receipt else {
                panic!("polling must observe an existing receipt, never insert one");
            };
            if state == "error" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("revoked queued request must reach a terminal error");
    assert!(
        fixture.seen.lock().is_empty(),
        "revoked queued work must never reach Agent execution"
    );
}

#[tokio::test]
async fn device_wire_policy_cannot_be_bypassed_through_anonymous_pairing_mode() {
    let fixture = DeviceFixture::with_pairing(&["chat"], false).await;
    for token in ["", DEVICE_TOKEN] {
        let (status, mut socket) = DeviceWire::upgrade(
            fixture.address,
            "agent=device-agent&session_id=device-session",
            token,
        )
        .await;
        assert_eq!(
            status, 503,
            "device policy needs an authenticated gateway even for a valid token"
        );
        // Upgrade refusal is an ordinary JSON HTTP body, not a WebSocket frame.
        let refusal = tokio::time::timeout(LIMIT, async {
            let mut body = Vec::new();
            loop {
                assert!(body.len() < 16_384, "oversized refusal body");
                body.push(socket.0.read_u8().await.unwrap());
                if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    break value;
                }
            }
        })
        .await
        .expect("bounded upgrade refusal body");
        assert_eq!(refusal["code"], "BRIDGE_BINDING_REJECTED");
    }
    assert!(fixture.seen.lock().is_empty());
    // Explicitly anonymous legacy installations still work with no device policy.
    fixture.state.config.write().gateway.bridges.clear();
    let mut anonymous = fixture.socket("").await;
    fixture
        .send_turn(&mut anonymous, "legacy-anonymous", "legacy anonymous chat")
        .await;
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn device_wire_live_policy_activation_closes_an_existing_anonymous_socket() {
    let fixture = DeviceFixture::with_pairing(&["chat"], false).await;
    let policy = fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .remove("device")
        .unwrap();
    let mut anonymous = fixture.socket("").await;
    assert!(fixture.seen.lock().is_empty());
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .insert("device".into(), policy);
    anonymous.send(json!({"type":"message", "id":"anonymous-after-policy", "content":"must not bypass device policy"})).await;
    anonymous.denied().await;
    assert!(
        fixture.seen.lock().is_empty(),
        "hot activation must not preserve anonymous operation authority"
    );
}

mod turn_control_tests {
    include!("turn_control_tests.rs");
}

fn owner_bridge(fixture: &DeviceFixture, operations: &[&str]) {
    let mut config = fixture.state.config.write();
    let bridge = config.gateway.bridges.get_mut("device").unwrap();
    bridge.device = None;
    bridge.owner = Some(reiclaw_config::schema::GatewayOwnerBridgeConfig {
        agent: "device-agent".into(),
        operations: operations.iter().map(|s| (*s).into()).collect(),
    });
}

#[tokio::test]
async fn owner_bridge_once_approval_is_explicit_and_never_always() {
    for decision in ["approve", "deny"] {
        let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
        owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
        let (status, mut client) = DeviceWire::upgrade(fixture.address, &format!("agent=device-agent&session_id={SESSION}"), DEVICE_TOKEN).await;
        assert_eq!(status, 101);
        client.until("session_start").await;
        client.send(json!({"type":"connect"})).await;
        assert_eq!(client.until("connected").await["owner_bridge_approval_once"], true);
        client.send(json!({"type":"turn_control","version":1,"op":"negotiate"})).await;
        let epoch = client.until("turn_ready").await["epoch"].clone();
        let id = uuid::Uuid::new_v4().to_string();
        client.send(json!({"type":"turn_control","version":1,"epoch":epoch,"op":"message","request_id":id,"content":"approval-probe"})).await;
        let approval = client.until("approval_request").await;
        client.send(json!({"type":"approval_response","request_id":approval["request_id"],"decision":"always"})).await;
        assert_eq!(client.until("error").await["code"], "BRIDGE_OPERATION_DENIED");
        assert!(!fixture.workspace.join("approved.txt").exists());
        client.send(json!({"type":"approval_response","request_id":approval["request_id"],"decision":decision})).await;
        loop { let event = client.until("turn_event").await; if event["terminal"] == true { assert_eq!(event["event"]["outcome"], "done"); break; } }
        assert_eq!(fixture.workspace.join("approved.txt").exists(), decision == "approve");
    }
}

#[tokio::test]
async fn owner_bridge_missing_grant_and_class_switch_do_not_elevate() {
    let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
    device.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"approve"})).await;
    device.denied().await;
    let mut owner = fixture.socket(DEVICE_TOKEN).await;
    fixture.state.config.write().gateway.bridges.get_mut("device").unwrap().owner.as_mut().unwrap().operations = vec!["chat".into()];
    owner.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"deny"})).await;
    owner.denied().await;
    let mut missing = fixture.socket(DEVICE_TOKEN).await;
    missing.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"approve"})).await;
    missing.denied().await;
}

#[tokio::test]
async fn owner_bridge_keeps_agent_workspace_and_auxiliary_surfaces_closed() {
    let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
    owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
    for query in ["agent=other-agent&session_id=device-session", "agent=device-agent&session_id=wrong", "agent=device-agent&session_id=device-session&cwd=%2Ftmp"] {
        assert_eq!(DeviceWire::upgrade(fixture.address, query, DEVICE_TOKEN).await.0, 403);
    }
    assert_eq!(DeviceWire::upgrade_path(fixture.address, "/ws/bridge", DEVICE_TOKEN).await.0, 401);
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    fixture.state.config.write().gateway.bridges.get_mut("device").unwrap().owner = None;
    client.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"approve"})).await;
    client.denied().await;
}

/// Opt-in real gateway fixture for desktop ACP acceptance. Uses temporary data,
/// the production Conversation/approval path and a deterministic HTTP provider.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual desktop acceptance; requires REI_ACP_ACCEPTANCE_DIR"]
async fn acp_desktop_gateway_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let directory = PathBuf::from(std::env::var_os("REI_ACP_ACCEPTANCE_DIR").expect("explicit output directory"));
    assert!(directory.is_absolute() && directory.is_dir());
    let address = std::fs::read_to_string(directory.join("bind-address")).unwrap_or_else(|_| "127.0.0.1:0".into()).trim().parse().unwrap();
    let fixture = DeviceFixture::with_address(&["chat", "cancel"], true, address).await;
    owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
    let token = std::fs::read_to_string(directory.join("credential")).unwrap_or_else(|_| format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()));
    {
        let mut config = fixture.state.config.write();
        let bridge = config.gateway.bridges.get_mut("device").unwrap();
        bridge.token_hash = reiclaw_config::tokens::hash_token(&token);
        bridge.session_prefix = Some("acp-".into());
    }
    let credential = directory.join("credential");
    std::fs::write(&credential, token).unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = format!("[acp_bridge]\nenabled = true\nendpoint = \"ws://{}/ws/chat\"\nagent = \"device-agent\"\nsession_prefix = \"acp-\"\ncredential_file = {:?}\n", fixture.address, credential);
    std::fs::write(directory.join("adapter.toml"), config).unwrap();
    std::fs::write(directory.join("ready.json"), json!({"address":fixture.address.to_string(),"workspace":fixture.workspace,"pid":std::process::id()}).to_string()).unwrap();
    for _ in 0..1800 {
        if directory.join("stop").exists() { break; }
        std::fs::write(directory.join("provider-count.json"), fixture.seen.lock().len().to_string()).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    std::fs::write(directory.join("provider-requests.json"), serde_json::to_vec_pretty(&*fixture.seen.lock()).unwrap()).unwrap();
}

#[tokio::test]
async fn owner_bridge_handshake_reports_cancel_only_when_granted() {
    for granted in [false, true] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        let operations = if granted { vec!["chat", "approval_response", "cancel"] } else { vec!["chat", "approval_response"] };
        owner_bridge(&fixture, &operations);
        let (status, mut client) = DeviceWire::upgrade(fixture.address, &format!("agent=device-agent&session_id={SESSION}"), DEVICE_TOKEN).await;
        assert_eq!(status, 101);
        client.until("session_start").await;
        client.send(json!({"type":"connect"})).await;
        let ack = client.until("connected").await;
        assert_eq!(ack["owner_bridge_approval_once"], true);
        assert_eq!(ack["owner_bridge_cancel"], granted);
    }
}


/// Paired clients must retain the configured agent workspace boundary.
#[tokio::test]
async fn paired_session_cwd_rejects_outside_workspace() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    fixture.state.config.write().gateway.bridges.clear();
    let (status, mut client) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session&cwd=%2F",
        OWNER_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    client.connected(json!({"type":"connect"})).await;
    client.send(json!({"type":"message", "id":"cwd-check", "content":"hello"})).await;
    let frame = client.next().await.unwrap();
    assert_eq!(frame["code"], "INVALID_CWD", "{frame}");
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn paired_session_cwd_accepts_workspace_descendant() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    fixture.state.config.write().gateway.bridges.clear();
    let workspace = fixture.state.config.read().agent_workspace_dir("device-agent");
    let child = workspace.join("project");
    std::fs::create_dir(&child).unwrap();
    let encoded_cwd: String = child.to_str().unwrap().bytes().map(|byte| format!("%{byte:02X}")).collect();
    let query = format!("agent=device-agent&session_id={SESSION}&cwd={encoded_cwd}");
    let (status, mut client) = DeviceWire::upgrade(fixture.address, &query, OWNER_TOKEN).await;
    assert_eq!(status, 101);
    client.connected(json!({"type":"connect"})).await;
    fixture.send_turn(&mut client, "cwd-child", "hello").await;
}

#[tokio::test]
async fn btw_wire_revoked_owner_is_refused_before_provider_or_slot_use() {
    for negotiated in [false, true] {
        let fixture = DeviceFixture::new(&["chat"]).await;
    fixture.state.config.write().gateway.bridges.clear();
        let mut client = fixture.socket(OWNER_TOKEN).await;
        if negotiated {
            client
                .send(json!({"type":"turn_control","version":1,"op":"negotiate"}))
                .await;
            client.until("turn_ready").await;
        }
        let accepted_id = format!("btw-{}", uuid::Uuid::new_v4());
        client
            .send(json!({"type":"btw","id":accepted_id,"content":"/btw before revocation"}))
            .await;
        let ack = client.until("ack").await;
        assert_eq!(ack["id"], accepted_id);
        assert_eq!(ack["status"], "accepted");
        assert_eq!(ack["turn"], "btw");
        assert_eq!(ack["durable"], false);
        let answer = client.until("btw_response").await;
        assert_eq!(answer["id"], accepted_id);
        assert_eq!(answer["content"], "device response");
        assert_eq!(fixture.seen.lock().len(), 1);

        let conversations = fixture
            .state
            .ws_conversations
            .session_conversations("gw_device-session");
        assert_eq!(conversations.len(), 1);
        let conversation = &conversations[0];
        let side_query = conversation.side_query.as_ref().unwrap();
        let revision = side_query.snapshot().revision;
        assert!(conversation.agent.lock().await.agent.history().is_empty());
        assert!(fixture.state.pairing.revoke_token(OWNER_TOKEN));

        let refused_id = format!("btw-{}", uuid::Uuid::new_v4());
        client
            .send(json!({"type":"btw","id":refused_id,"content":"/btw after revocation"}))
            .await;
        let refused = client.next().await.expect("correlated revocation refusal");
        assert_eq!(refused["type"], "error");
        assert_eq!(refused["id"], refused_id);
        assert_eq!(refused["code"], "BRIDGE_OPERATION_DENIED");
        assert_eq!(fixture.seen.lock().len(), 1);
        let permits: Vec<_> = (0..reiclaw_runtime::agent::side_query::MAX_CONCURRENT_SIDE_QUERIES)
            .map(|_| {
                side_query
                    .try_acquire_slot()
                    .expect("refusal consumes no slot")
            })
            .collect();
        assert!(side_query.try_acquire_slot().is_none());
        drop(permits);
        assert_eq!(side_query.snapshot().revision, revision);
        assert!(!conversation.is_running());
        assert!(conversation.agent.lock().await.agent.history().is_empty());
        assert!(
            fixture
                .state
                .session_backend
                .as_ref()
                .unwrap()
                .load("gw_device-session")
                .is_empty()
        );
    }
}

#[tokio::test]
async fn btw_wire_bridge_requires_its_live_chat_grant() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    assert!(fixture.state.pairing.revoke_token(DEVICE_TOKEN));
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    client
        .send(json!({"type":"turn_control","version":1,"op":"negotiate"}))
        .await;
    client.until("turn_ready").await;
    client
        .send(json!({"type":"btw","id":"btw-bridge-allowed","content":"/btw bridge question"}))
        .await;
    assert_eq!(client.until("ack").await["id"], "btw-bridge-allowed");
    assert_eq!(
        client.until("btw_response").await["content"],
        "device response"
    );
    assert_eq!(fixture.seen.lock().len(), 1);
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .get_mut("device")
        .unwrap()
        .device
        .as_mut()
        .unwrap()
        .operations
        .clear();
    client
        .send(json!({"type":"btw","id":"btw-bridge-refused","content":"/btw must not run"}))
        .await;
    client.denied().await;
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn btw_wire_anonymous_legacy_chat_retains_pairing_off_policy() {
    let fixture = DeviceFixture::with_pairing(&["chat"], false).await;
    fixture.state.config.write().gateway.bridges.clear();
    let mut client = fixture.socket("").await;
    client
        .send(json!({"type":"btw","id":"btw-anonymous","content":"/btw anonymous question"}))
        .await;
    let ack = client.until("ack").await;
    assert_eq!(ack["id"], "btw-anonymous");
    assert_eq!(ack["status"], "accepted");
    assert_eq!(
        client.until("btw_response").await["content"],
        "device response"
    );
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn device_full_agent_is_confined_on_every_non_voice_surface() {
    for surface in ["", "&surface=web", "&surface=telegram", "&surface=cli"] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        {
            let mut config = fixture.state.config.write();
            config.risk_profiles.get_mut("test").unwrap().level = reiclaw_config::autonomy::AutonomyLevel::Full;
            config.risk_profiles.get_mut("test").unwrap().allowed_tools = None;
            let public = config.risk_profiles.get_mut("public-device").unwrap();
            // Keep one real public tool registered so the provider's adversarial
            // native calls reach dispatch; the skill-shaped name must stay absent.
            public.allowed_tools = Some(vec!["web_search_tool".into(), "public__probe".into()]);
            public.auto_approve = vec!["web_search_tool".into(), "public__probe".into()];
            config.web_search.enabled = true;
            config.memory.auto_save = true;
            config.agents.get_mut("device-agent").unwrap().memory.backend = reiclaw_config::multi_agent::MemoryBackendKind::Markdown;
        }
        let skill_dir = fixture.workspace.join("skills/public");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.toml"), r#"
[skill]
name = "public"
description = "PRIVATE_SKILL_SENTINEL"
version = "0.1.0"
[[tools]]
name = "probe"
description = "Synthetic private HTTP action"
kind = "http"
command = "http://127.0.0.1:1/private"
"#).unwrap();
        let config = fixture.state.config.read().clone();
        let ordinary = reiclaw_runtime::agent::Agent::from_config_with_session_cwd_and_mcp(
            &config, "device-agent", Some(&fixture.workspace), false).await.unwrap();
        assert!(ordinary.tool_names().contains(&"public__probe"), "ordinary skills remain registered");
        let memory = resolve_ws_memory_handle(&config, "device-agent").await.unwrap().unwrap();
        memory.store("owner-private", "PRIVATE_RECALL_SENTINEL device-tool-probe",
            reiclaw_memory::MemoryCategory::Core, None).await.unwrap();
        let memory_before = memory.count().await.unwrap();
        // Actual HTTP provider sees the constructed agent; no injected registry.
        let (status, mut client) = DeviceWire::upgrade(fixture.address,
            &format!("agent=device-agent&session_id={SESSION}{surface}"), DEVICE_TOKEN).await;
        assert_eq!(status, 101);
        client.connected(json!({"type":"connect"})).await;
        fixture.send_turn(&mut client, "confinement", "device-tool-probe").await;
        assert_eq!(memory.count().await.unwrap(), memory_before, "device turns must not write memory");
        let seen = fixture.seen.lock();
        assert_eq!(seen.len(), 2, "one tool round and final answer, no consolidation");
        assert!(!seen[0]["input"].to_string().contains("PRIVATE_RECALL_SENTINEL"));
        assert!(!seen[0]["input"].to_string().contains("PRIVATE_SKILL_SENTINEL"));
        for name in ["shell", "file_write", "notify", "memory_store", "sessions_send", "cron_add", "public__probe"] {
            assert!(!seen[0]["tools"].to_string().contains(name));
            assert!(seen[1]["input"].to_string().contains(&format!("Unknown tool: {name}")));
        }
        assert!(!fixture.workspace.join("forbidden.txt").exists());
        assert!(fixture.state.session_backend.as_ref().unwrap().load(&format!("gw_{SESSION}")).iter().any(|m| m.role == "user"));
    }
}

#[tokio::test]
async fn device_namespace_refuses_owner_legacy_bridge_and_connect_override() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    for session in [SESSION, "outside-device"] {
        let token = if session == SESSION { OWNER_TOKEN } else { DEVICE_TOKEN };
        let (status, _) = DeviceWire::upgrade(fixture.address,
            &format!("agent=device-agent&session_id={session}"), token).await;
        assert_eq!(status, 403);
    }
    fixture.state.config.write().gateway.bridges.insert("legacy".into(), GatewayBridgeConfig {
        token_hash: reiclaw_config::tokens::hash_token("synthetic-legacy-token"),
        sessions: vec![SESSION.into()], ..Default::default()
    });
    let (status, _) = DeviceWire::upgrade(fixture.address,
        &format!("agent=device-agent&session_id={SESSION}"), "synthetic-legacy-token").await;
    assert_eq!(status, 403);
    let (status, mut owner) = DeviceWire::upgrade(fixture.address,
        "agent=device-agent&session_id=owner-session", OWNER_TOKEN).await;
    assert_eq!(status, 101);
    owner.until("session_start").await;
    owner.send(json!({"type":"connect", "session_id":SESSION})).await;
    assert_eq!(owner.until("error").await["code"], "BRIDGE_BINDING_REJECTED");
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn device_turn_prompt_excludes_private_context_but_owner_keeps_it() {
    use reiclaw_memory::companion::*;
    let fixture = DeviceFixture::new(&["chat"]).await;
    let config = fixture.state.config.read().clone();
    let soul = SoulProfileStore::shared(&config.data_dir).unwrap();
    soul.set_identity("device-agent", SoulIdentity {
        name: "Same Identity".into(), self_description: None, primary_language: None, pronouns: None,
    }, 0, 1).unwrap();
    soul.set_story("device-agent", SoulStory { entries: vec![StoryEntry { kind: StoryKind::Bond, text: "PRIVATE_STORY_SENTINEL".into() }] }, 0, 2).unwrap();
    soul.set_tone_exemplars("device-agent", SoulToneExemplars { exemplars: vec![ToneExemplar {
        situation: "Private situation".into(), owner_line: "Private line".into(), agent_line: "PRIVATE_EXEMPLAR_SENTINEL".into(),
    }] }, 0, 2).unwrap();
    UserModelStore::shared(&config.data_dir).unwrap().record_owner_statement(
        UserModelKind::Preference, "PRIVATE_USER_MODEL_SENTINEL", "style", "global", 1).unwrap();
    std::fs::write(fixture.workspace.join("USER.md"), "PRIVATE_USER_FILE_SENTINEL").unwrap();
    std::fs::write(fixture.workspace.join("MEMORY.md"), "PRIVATE_MEMORY_FILE_SENTINEL").unwrap();
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    fixture.send_turn(&mut device, "first", "public question").await;
    fixture.send_turn(&mut device, "second", "another public question").await;
    {
        let seen = fixture.seen.lock();
        for request in seen.iter() {
            let prompt = request["instructions"].as_str().unwrap();
            assert!(prompt.contains("Same Identity"));
            assert!(prompt.contains("## Principles"));
            assert!(prompt.contains("## Tone"));
            for marker in ["PRIVATE_STORY_SENTINEL", "PRIVATE_EXEMPLAR_SENTINEL", "PRIVATE_USER_MODEL_SENTINEL", "PRIVATE_USER_FILE_SENTINEL", "PRIVATE_MEMORY_FILE_SENTINEL"] {
                assert!(!prompt.contains(marker), "leaked {marker}");
            }
        }
    }
    let mut owner = fixture.socket(OWNER_TOKEN).await;
    fixture.send_turn(&mut owner, "owner", "owner question").await;
    let seen = fixture.seen.lock();
    let owner_prompt = seen.last().unwrap()["instructions"].as_str().unwrap();
    for marker in ["Same Identity", "PRIVATE_STORY_SENTINEL", "PRIVATE_EXEMPLAR_SENTINEL", "PRIVATE_USER_MODEL_SENTINEL"] {
        assert!(owner_prompt.contains(marker), "owner regression: {marker}");
    }
}

#[tokio::test]
async fn device_tool_rounds_stop_at_three_or_the_agents_lower_limit() {
    for limit in [2, 7] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        {
            let mut config = fixture.state.config.write();
            config.runtime_profiles.get_mut("test").unwrap().max_tool_iterations = limit;
            let public = config.risk_profiles.get_mut("public-device").unwrap();
            public.allowed_tools = Some(vec!["web_search_tool".into()]);
            public.auto_approve = vec!["web_search_tool".into()];
            config.web_search.enabled = true;
        }
        let mut client = fixture.socket(DEVICE_TOKEN).await;
        client.send(json!({"type":"message", "id":"cap", "content":"device-cap-probe"})).await;
        let error = client.until("error").await;
        assert_eq!(error["code"], "AGENT_ERROR");
        assert!(error["message"].as_str().unwrap().contains("maximum tool iterations"));
        let seen = fixture.seen.lock();
        // Amendment 1 caps tool rounds. The streaming path then makes exactly
        // one tools-free summary request; it cannot execute another tool round.
        let (summary, rounds) = seen.split_last().unwrap();
        assert_eq!(rounds.len(), limit.min(3));
        for round in rounds {
            assert_eq!(round["tools"].as_array().unwrap().len(), 1);
            assert_eq!(round["tools"][0]["name"], "web_search_tool");
        }
        assert!(summary["tools"].is_null());
        assert!(summary["input"].as_array().unwrap().last().unwrap()["content"]
            .to_string().contains("maximum number of tool iterations"));
    }
}

#[tokio::test]
async fn device_profile_reload_revokes_the_old_agent_snapshot() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    fixture.send_turn(&mut client, "before", "before reload").await;
    {
        let mut config = fixture.state.config.write();
        let profile = config.risk_profiles.get_mut("public-device").unwrap();
        profile.allowed_tools = Some(vec!["web_search_tool".into()]);
        profile.auto_approve = vec!["web_search_tool".into()];
    }
    client.send(json!({"type":"message", "id":"after", "content":"must not use old policy"})).await;
    client.denied().await;
    assert_eq!(fixture.seen.lock().len(), 1);
}
