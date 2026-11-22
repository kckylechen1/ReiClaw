use async_trait::async_trait;
use futures_util::{StreamExt, stream::BoxStream};
use reiclaw_api::agent::TurnEvent;
use reiclaw_api::attribution::{Attributable, Role};
use reiclaw_api::channel::{
    Channel, ChannelApprovalRequest, ChannelApprovalResponse, ChannelMessage, SendMessage,
};
use reiclaw_api::ingress::IngressContext;
use reiclaw_api::model_provider::{
    ChatRequest, ProviderCapabilities, StreamChunk, StreamEvent, StreamOptions, StreamResult,
};
use reiclaw_api::tool::{Tool, ToolOutput, ToolResult};
use reiclaw_config::schema::{EstopConfig, MultimodalConfig, PacingConfig, RiskProfileConfig};
use reiclaw_providers::{ChatMessage, ChatResponse, ModelProvider, ToolCall};
use reiclaw_runtime::agent::loop_::{
    LoopKnobs, ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess, ResolvedRuntimeKnobs,
    ToolLoop, run_tool_call_loop_with_security,
};
use reiclaw_runtime::approval::ApprovalManager;
use reiclaw_runtime::hooks::HookRunner;
use reiclaw_runtime::hooks::{HookHandler, HookResult};
use reiclaw_runtime::observability::NoopObserver;
use reiclaw_runtime::security::SecurityPolicy;
use reiclaw_runtime::security::estop::EstopState;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct RecordingTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

impl Attributable for RecordingTool {
    fn role(&self) -> Role {
        Role::System
    }

    fn alias(&self) -> &str {
        self.name
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "recording test tool"
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: ToolOutput::text("ok"),
            error: None,
        })
    }
}

struct StubProvider {
    chat_calls: Arc<AtomicUsize>,
    next_calls: Mutex<Option<Vec<ToolCall>>>,
}

impl Attributable for StubProvider {
    fn role(&self) -> Role {
        Role::System
    }

    fn alias(&self) -> &str {
        "estop-stub-provider"
    }
}

#[async_trait]
impl ModelProvider for StubProvider {
    fn endpoint_host(&self) -> Option<String> {
        Some("parent.example".into())
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            ..Default::default()
        }
    }

    async fn chat_with_system(
        &self,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<String> {
        anyhow::bail!("unused")
    }

    async fn chat(
        &self,
        _: ChatRequest<'_>,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        let calls = self.next_calls.lock().unwrap().take();
        Ok(ChatResponse {
            text: calls.is_none().then(|| "done".into()),
            tool_calls: calls.unwrap_or_default(),
            usage: None,
            reasoning_content: None,
        })
    }
}

struct PendingStreamProvider {
    stream_calls: AtomicUsize,
    fallback_calls: AtomicUsize,
    chunks: Vec<&'static str>,
    last_chunk_polled: Arc<tokio::sync::Notify>,
}

impl Attributable for PendingStreamProvider {
    fn role(&self) -> Role {
        Role::System
    }

    fn alias(&self) -> &str {
        "estop-pending-stream"
    }
}

#[async_trait]
impl ModelProvider for PendingStreamProvider {
    fn endpoint_host(&self) -> Option<String> {
        // The domain-stop message also resembles a context overflow. Estop
        // must remain terminal instead of entering context-recovery retries.
        Some("context_length_exceeded.example".into())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<String> {
        anyhow::bail!("unused")
    }

    async fn chat(
        &self,
        _: ChatRequest<'_>,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        self.fallback_calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("estop must not fall back to non-streaming chat")
    }

    fn stream_chat(
        &self,
        _: ChatRequest<'_>,
        _: &str,
        _: Option<f64>,
        _: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamEvent>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let chunk_count = self.chunks.len();
        let last_chunk_polled = Arc::clone(&self.last_chunk_polled);
        futures_util::stream::iter(self.chunks.clone().into_iter().enumerate().map(
            move |(index, text)| {
                if index + 1 == chunk_count {
                    last_chunk_polled.notify_one();
                }
                Ok(StreamEvent::TextDelta(StreamChunk::delta(text)))
            },
        ))
        .chain(futures_util::stream::pending())
        .boxed()
    }
}

struct RewriteToFrozenHook;

#[async_trait]
impl HookHandler for RewriteToFrozenHook {
    fn name(&self) -> &str {
        "rewrite-to-frozen"
    }

    async fn before_tool_call(&self, _name: String, args: Value) -> HookResult<(String, Value)> {
        HookResult::Continue(("frozen_target".to_string(), args))
    }
}

struct PanicIfPromptedChannel;

impl Attributable for PanicIfPromptedChannel {
    fn role(&self) -> Role {
        Role::System
    }

    fn alias(&self) -> &str {
        "panic-channel"
    }
}

#[async_trait]
impl Channel for PanicIfPromptedChannel {
    fn name(&self) -> &str {
        "test"
    }

    async fn send(&self, _: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, _: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn request_approval(
        &self,
        _: &str,
        _: &ChannelApprovalRequest,
    ) -> anyhow::Result<Option<ChannelApprovalResponse>> {
        panic!("frozen tool must be blocked before approval prompt");
    }
}

fn write_state(path: &std::path::Path, state: &EstopState) {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(state).unwrap()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn security_with_estop(
    dir: &std::path::Path,
    initial: &EstopState,
) -> (std::path::PathBuf, SecurityPolicy) {
    let state_path = dir.join("estop-state.json");
    let cfg = EstopConfig {
        enabled: true,
        state_file: state_path.to_string_lossy().to_string(),
        require_otp_to_resume: false,
    };
    let security = SecurityPolicy {
        workspace_dir: dir.to_path_buf(),
        ..SecurityPolicy::default()
    }
    .with_estop(&cfg, dir);
    write_state(&state_path, initial);
    (state_path, security)
}

#[tokio::test]
async fn kill_all_aborts_turn_before_provider_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("estop-state.json");
    let cfg = EstopConfig {
        enabled: true,
        state_file: state_path.to_string_lossy().to_string(),
        require_otp_to_resume: false,
    };
    let security = SecurityPolicy {
        workspace_dir: dir.path().to_path_buf(),
        ..SecurityPolicy::default()
    }
    .with_estop(&cfg, dir.path());

    write_state(
        &state_path,
        &EstopState {
            kill_all: true,
            network_kill: false,
            blocked_domains: Vec::new(),
            frozen_tools: Vec::new(),
            updated_at: None,
        },
    );

    let chat_calls = Arc::new(AtomicUsize::new(0));
    let provider = StubProvider {
        chat_calls: Arc::clone(&chat_calls),
        next_calls: Mutex::new(None),
    };
    let mut history = vec![ChatMessage::user("hello")];

    let err = run_tool_call_loop_with_security(
        ToolLoop {
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: &provider,
                    provider_name: "stub",
                    model: "stub",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: &[],
                    observer: &NoopObserver,
                    silent: true,
                    approval: None,
                    multimodal_config: &MultimodalConfig::default(),
                    config: None,
                    hooks: None,
                    activated_tools: None,
                    receipt_generator: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations: 3,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &PacingConfig::default(),
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 0,
                    keep_tool_context_turns: 2,
                    context_token_budget: 0,
                    knobs: &LoopKnobs::default(),
                },
            ),
            history: &mut history,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            shared_budget: None,
            channel: None,
            collected_receipts: None,
            event_tx: None,
            steering: None,
            new_messages_out: None,
            image_cache: None,
            ingress: IngressContext::sub_turn(),
            memory: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "estop-kill-all-test",
        },
        Some(&security),
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("kill_all"));
    assert_eq!(chat_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn frozen_tools_blocks_after_hook_rewrite_before_approval_and_execution() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("estop-state.json");
    let cfg = EstopConfig {
        enabled: true,
        state_file: state_path.to_string_lossy().to_string(),
        require_otp_to_resume: false,
    };
    let security = SecurityPolicy {
        workspace_dir: dir.path().to_path_buf(),
        ..SecurityPolicy::default()
    }
    .with_estop(&cfg, dir.path());

    write_state(
        &state_path,
        &EstopState {
            kill_all: false,
            network_kill: false,
            blocked_domains: Vec::new(),
            frozen_tools: vec!["frozen_target".into()],
            updated_at: None,
        },
    );

    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "frozen_target",
        calls: Arc::clone(&tool_calls),
    })];
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let provider = StubProvider {
        chat_calls: Arc::clone(&chat_calls),
        next_calls: Mutex::new(Some(vec![ToolCall {
            id: "c1".into(),
            name: "safe_alias".into(),
            arguments: "{}".into(),
            extra_content: None,
        }])),
    };
    let mut hooks = HookRunner::new();
    hooks.register(Box::new(RewriteToFrozenHook));
    let approval = ApprovalManager::for_non_interactive(&RiskProfileConfig {
        always_ask: vec!["frozen_target".into()],
        ..Default::default()
    });
    let mut history = vec![ChatMessage::user("run")];

    let out = run_tool_call_loop_with_security(
        ToolLoop {
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: &provider,
                    provider_name: "stub",
                    model: "stub",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: &tools,
                    observer: &NoopObserver,
                    silent: true,
                    approval: Some(&approval),
                    multimodal_config: &MultimodalConfig::default(),
                    config: None,
                    hooks: Some(&hooks),
                    activated_tools: None,
                    receipt_generator: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations: 3,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &PacingConfig::default(),
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 0,
                    keep_tool_context_turns: 2,
                    context_token_budget: 0,
                    knobs: &LoopKnobs::default(),
                },
            ),
            history: &mut history,
            channel_name: "test",
            channel_reply_target: Some("target"),
            cancellation_token: None,
            on_delta: None,
            shared_budget: None,
            channel: Some(&PanicIfPromptedChannel),
            collected_receipts: None,
            event_tx: None,
            steering: None,
            new_messages_out: None,
            image_cache: None,
            ingress: IngressContext::sub_turn(),
            memory: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "estop-frozen-hook-rewrite",
        },
        Some(&security),
    )
    .await
    .unwrap();

    assert_eq!(out, "done");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn remote_provider_network_kill_blocks_before_http_request() {
    use reiclaw_providers::openai::OpenAiResponsesModelProvider;

    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let provider = OpenAiResponsesModelProvider::builder("remote-test")
        .api_url(&server.uri())
        .credential(Some("sk-test"))
        .build();

    let dir = tempfile::tempdir().unwrap();
    let (_state_path, security) = security_with_estop(
        dir.path(),
        &EstopState {
            kill_all: false,
            network_kill: true,
            blocked_domains: Vec::new(),
            frozen_tools: Vec::new(),
            updated_at: None,
        },
    );

    // 1) Non-streaming path
    let mut history_chat = vec![ChatMessage::user("hi")];
    let err_chat = run_tool_call_loop_with_security(
        ToolLoop {
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: &provider,
                    provider_name: "remote-test",
                    model: "gpt-4o-mini",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: &[],
                    observer: &NoopObserver,
                    silent: true,
                    approval: None,
                    multimodal_config: &MultimodalConfig::default(),
                    config: None,
                    hooks: None,
                    activated_tools: None,
                    receipt_generator: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations: 1,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &PacingConfig::default(),
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 0,
                    keep_tool_context_turns: 2,
                    context_token_budget: 0,
                    knobs: &LoopKnobs::default(),
                },
            ),
            history: &mut history_chat,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            shared_budget: None,
            channel: None,
            collected_receipts: None,
            event_tx: None,
            steering: None,
            new_messages_out: None,
            image_cache: None,
            ingress: IngressContext::sub_turn(),
            memory: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "remote-net-kill-chat",
        },
        Some(&security),
    )
    .await
    .expect_err("network_kill must block non-streaming remote provider");
    assert!(err_chat.to_string().contains("network_kill"));

    // 2) Streaming path
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let mut history_stream = vec![ChatMessage::user("hi")];
    let err_stream = run_tool_call_loop_with_security(
        ToolLoop {
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: &provider,
                    provider_name: "remote-test",
                    model: "gpt-4o-mini",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: &[],
                    observer: &NoopObserver,
                    silent: true,
                    approval: None,
                    multimodal_config: &MultimodalConfig::default(),
                    config: None,
                    hooks: None,
                    activated_tools: None,
                    receipt_generator: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations: 1,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &PacingConfig::default(),
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 0,
                    keep_tool_context_turns: 2,
                    context_token_budget: 0,
                    knobs: &LoopKnobs::default(),
                },
            ),
            history: &mut history_stream,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: Some(tx),
            shared_budget: None,
            channel: None,
            collected_receipts: None,
            event_tx: None,
            steering: None,
            new_messages_out: None,
            image_cache: None,
            ingress: IngressContext::sub_turn(),
            memory: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "remote-net-kill-stream",
        },
        Some(&security),
    )
    .await
    .expect_err("network_kill must block streaming remote provider");
    assert!(err_stream.to_string().contains("network_kill"));
}

#[tokio::test]
async fn remote_provider_midflight_kill_all_aborts_streaming_and_non_streaming() {
    use reiclaw_providers::openai::OpenAiResponsesModelProvider;

    for use_streaming in [false, true] {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)),
            )
            .mount(&server)
            .await;

        let provider = OpenAiResponsesModelProvider::builder("remote-midflight")
            .api_url(&server.uri())
            .credential(Some("sk-test"))
            .build();

        let dir = tempfile::tempdir().unwrap();
        let (state_path, security) = security_with_estop(dir.path(), &EstopState::default());

        let writer = state_path.clone();
        let engager = ::reiclaw_log::spawn!(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            let engaged = EstopState {
                kill_all: true,
                network_kill: false,
                blocked_domains: Vec::new(),
                frozen_tools: Vec::new(),
                updated_at: None,
            };
            let tmp = writer.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string(&engaged).unwrap()).unwrap();
            std::fs::rename(&tmp, &writer).unwrap();
        });

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let on_delta = use_streaming.then_some(tx);
        let mut history = vec![ChatMessage::user("hi")];
        let started = std::time::Instant::now();
        let err = run_tool_call_loop_with_security(
            ToolLoop {
                exec: ResolvedAgentExecution::resolve(
                    ResolvedModelAccess {
                        model_provider: &provider,
                        provider_name: "remote-midflight",
                        model: "gpt-4o-mini",
                        temperature: None,
                    },
                    ResolvedIo {
                        tools_registry: &[],
                        observer: &NoopObserver,
                        silent: true,
                        approval: None,
                        multimodal_config: &MultimodalConfig::default(),
                        config: None,
                        hooks: None,
                        activated_tools: None,
                        receipt_generator: None,
                    },
                    ResolvedRuntimeKnobs {
                        max_tool_iterations: 1,
                        excluded_tools: &[],
                        dedup_exempt_tools: &[],
                        pacing: &PacingConfig::default(),
                        strict_tool_parsing: false,
                        parallel_tools: false,
                        max_tool_result_chars: 0,
                        keep_tool_context_turns: 2,
                        context_token_budget: 0,
                        knobs: &LoopKnobs::default(),
                    },
                ),
                history: &mut history,
                channel_name: "test",
                channel_reply_target: None,
                cancellation_token: None,
                on_delta,
                shared_budget: None,
                channel: None,
                collected_receipts: None,
                event_tx: None,
                steering: None,
                new_messages_out: None,
                image_cache: None,
                ingress: IngressContext::sub_turn(),
                memory: None,
                agent_alias: None,
                parent_agent_alias: None,
                turn_id: "remote-midflight-kill",
            },
            Some(&security),
        )
        .await
        .expect_err("mid-flight kill_all must abort remote provider wait");
        engager.await.unwrap();
        assert!(err.to_string().contains("kill_all"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "mid-flight abort took {:?}",
            started.elapsed()
        );
    }
}

async fn run_estop_regression_turn(
    provider: &dyn ModelProvider,
    tools: &[Box<dyn Tool>],
    security: &SecurityPolicy,
    history: &mut Vec<ChatMessage>,
    new_messages: &mut Vec<ChatMessage>,
    event_tx: Option<tokio::sync::mpsc::Sender<TurnEvent>>,
) -> anyhow::Result<String> {
    run_tool_call_loop_with_security(
        ToolLoop {
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: provider,
                    provider_name: "estop-regression-parent",
                    model: "parent-model",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: tools,
                    observer: &NoopObserver,
                    silent: true,
                    approval: None,
                    multimodal_config: &MultimodalConfig::default(),
                    config: None,
                    hooks: None,
                    activated_tools: None,
                    receipt_generator: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations: 3,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &PacingConfig::default(),
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 0,
                    keep_tool_context_turns: 2,
                    context_token_budget: 0,
                    knobs: &LoopKnobs::default(),
                },
            ),
            history,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            shared_budget: None,
            channel: None,
            collected_receipts: None,
            event_tx,
            steering: None,
            new_messages_out: Some(new_messages),
            image_cache: None,
            ingress: IngressContext::sub_turn(),
            memory: None,
            agent_alias: Some("parent"),
            parent_agent_alias: None,
            turn_id: "estop-regression",
        },
        Some(security),
    )
    .await
}

fn advisor_registry(uri: &str, security: Arc<SecurityPolicy>) -> Vec<Box<dyn Tool>> {
    use reiclaw_config::schema::{AliasedAgentConfig, Config, WireApi};
    use reiclaw_runtime::subagent_v1::ReasoningSubagentTool;

    let mut config = Config::default();
    for (alias, endpoint) in [("parent", "https://parent.example"), ("advisor", uri)] {
        let entry = config.providers.models.ensure("openai", alias).unwrap();
        entry.api_key = Some("test-key".into());
        entry.uri = Some(endpoint.into());
        entry.model = Some(format!("{alias}-model"));
        entry.wire_api = Some(WireApi::Responses);
    }
    config
        .risk_profiles
        .insert("default".into(), RiskProfileConfig::default());
    config.agents.insert(
        "parent".into(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            model_provider: "openai.parent".into(),
            ..Default::default()
        },
    );
    vec![Box::new(
        ReasoningSubagentTool::new(Arc::new(config), "parent", security)
            .with_advisor("openai.advisor"),
    )]
}

fn advisor_parent() -> StubProvider {
    StubProvider {
        chat_calls: Arc::new(AtomicUsize::new(0)),
        next_calls: Mutex::new(Some(vec![ToolCall {
            id: "advisor-call".into(),
            name: "reasoning_subagent".into(),
            arguments: json!({"objective": "Review this bounded objective"}).to_string(),
            extra_content: None,
        }])),
    }
}

#[tokio::test]
async fn advisor_checks_its_resolved_domain_while_parent_domain_is_allowed() {
    for blocked in [false, true] {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "output_text": "{\"summary\":\"advisor completed\"}",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (_, security) = security_with_estop(
            dir.path(),
            &EstopState {
                blocked_domains: if blocked {
                    vec!["127.0.0.1".into()]
                } else {
                    vec![]
                },
                ..Default::default()
            },
        );
        let security = Arc::new(security);
        let tools = advisor_registry(&server.uri(), Arc::clone(&security));
        let provider = advisor_parent();
        let mut history = vec![ChatMessage::user("consult the advisor")];
        let mut new_messages = Vec::new();
        let answer = run_estop_regression_turn(
            &provider,
            &tools,
            &security,
            &mut history,
            &mut new_messages,
            None,
        )
        .await
        .expect("the parent domain remains allowed");
        assert_eq!(answer, "done");
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            usize::from(!blocked)
        );
        let tool_result = history.iter().find(|msg| msg.role == "tool").unwrap();
        if blocked {
            assert!(
                tool_result.content.contains("blocked by emergency stop"),
                "{}",
                tool_result.content
            );
            assert!(tool_result.content.contains("127.0.0.1"));
            assert!(!tool_result.content.contains("advisor completed"));
        } else {
            assert!(
                tool_result.content.contains("advisor completed"),
                "{}",
                tool_result.content
            );
        }
    }
}

#[tokio::test]
async fn advisor_inflight_request_observes_live_estop() {
    for network_only in [false, true] {
        let server = wiremock::MockServer::start().await;
        let requested = Arc::new(tokio::sync::Notify::new());
        let request_signal = Arc::clone(&requested);
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                request_signal.notify_one();
                wiremock::ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30))
            })
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (state_path, security) = security_with_estop(dir.path(), &EstopState::default());
        let security = Arc::new(security);
        let tools = advisor_registry(&server.uri(), Arc::clone(&security));
        let provider = advisor_parent();
        let mut history = vec![ChatMessage::user("consult the advisor")];
        let mut new_messages = Vec::new();
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(
                run_estop_regression_turn(
                    &provider,
                    &tools,
                    &security,
                    &mut history,
                    &mut new_messages,
                    None,
                ),
                async {
                    requested.notified().await;
                    write_state(
                        &state_path,
                        &EstopState {
                            kill_all: !network_only,
                            network_kill: network_only,
                            ..Default::default()
                        },
                    );
                },
            )
        })
        .await
        .expect("live estop must abort the pending advisor request");
        let reason = if network_only {
            "network_kill"
        } else {
            "kill_all"
        };
        assert!(result.unwrap_err().to_string().contains(reason));
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        if network_only {
            assert!(
                history
                    .iter()
                    .any(|msg| msg.role == "tool" && msg.content.contains(reason)),
                "advisor network stop must be reported as a failed tool result: {history:?}"
            );
        }
    }
}

#[tokio::test]
async fn midstream_estop_preserves_only_delivered_text_without_retry() {
    for state in [
        EstopState {
            kill_all: true,
            ..Default::default()
        },
        EstopState {
            network_kill: true,
            ..Default::default()
        },
        EstopState {
            blocked_domains: vec!["*.example".into()],
            ..Default::default()
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (state_path, security) = security_with_estop(dir.path(), &EstopState::default());
        let last_chunk_polled = Arc::new(tokio::sync::Notify::new());
        let provider = PendingStreamProvider {
            stream_calls: AtomicUsize::new(0),
            fallback_calls: AtomicUsize::new(0),
            chunks: vec![
                "Visible answer.",
                "<think>private reasoning</think><tool_call",
            ],
            last_chunk_polled: Arc::clone(&last_chunk_polled),
        };
        // Include an older turn: a domain whose name resembles a context error
        // must not trigger recovery, trim history, or drop the visible partial.
        let mut history = vec![
            ChatMessage::user("old question"),
            ChatMessage::assistant("old answer"),
            ChatMessage::user("new question"),
        ];
        let mut new_messages = Vec::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(
                run_estop_regression_turn(
                    &provider,
                    &[],
                    &security,
                    &mut history,
                    &mut new_messages,
                    Some(event_tx),
                ),
                async {
                    last_chunk_polled.notified().await;
                    assert!(matches!(event_rx.recv().await,
                        Some(TurnEvent::Chunk { delta }) if delta == "Visible answer."));
                    write_state(&state_path, &state);
                },
            )
        })
        .await
        .expect("estop must abort a pending stream");
        assert!(result.unwrap_err().to_string().contains("emergency stop"));
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.fallback_calls.load(Ordering::SeqCst), 0);
        let expected = format!(
            "Visible answer.\n\n{}",
            reiclaw_runtime::agent::STREAM_INTERRUPTED_MARKER
        );
        assert_eq!(history.len(), 4, "estop must not trim prior history");
        assert_eq!(history.last().unwrap().content, expected);
        assert_eq!(new_messages.len(), 1);
        assert_eq!(new_messages[0].role, "assistant");
        assert_eq!(new_messages[0].content, expected);
        assert!(
            event_rx.try_recv().is_err(),
            "no extra output or successful completion"
        );
    }
}

#[tokio::test]
async fn midstream_estop_aborts_backpressure_without_committing_unsent_text() {
    let dir = tempfile::tempdir().unwrap();
    let (state_path, security) = security_with_estop(dir.path(), &EstopState::default());
    let last_chunk_polled = Arc::new(tokio::sync::Notify::new());
    let provider = PendingStreamProvider {
        stream_calls: AtomicUsize::new(0),
        fallback_calls: AtomicUsize::new(0),
        chunks: vec!["Delivered.", "Unsent."],
        last_chunk_polled: Arc::clone(&last_chunk_polled),
    };
    let mut history = vec![ChatMessage::user("go")];
    let mut new_messages = Vec::new();
    // The second send cannot finish until the receiver drains the first.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(
            run_estop_regression_turn(
                &provider,
                &[],
                &security,
                &mut history,
                &mut new_messages,
                Some(event_tx),
            ),
            async {
                last_chunk_polled.notified().await;
                write_state(
                    &state_path,
                    &EstopState {
                        kill_all: true,
                        ..Default::default()
                    },
                );
            },
        )
    })
    .await
    .expect("estop must abort even while the event sink is full");
    assert!(result.unwrap_err().to_string().contains("kill_all"));
    let expected = format!(
        "Delivered.\n\n{}",
        reiclaw_runtime::agent::STREAM_INTERRUPTED_MARKER
    );
    assert_eq!(history.last().unwrap().content, expected);
    assert_eq!(new_messages.len(), 1);
    assert_eq!(new_messages[0].content, expected);
    assert!(matches!(event_rx.try_recv(), Ok(TurnEvent::Chunk { delta }) if delta == "Delivered."));
    assert!(event_rx.try_recv().is_err());
    assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.fallback_calls.load(Ordering::SeqCst), 0);
}
