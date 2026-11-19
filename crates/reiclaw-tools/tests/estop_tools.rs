use async_trait::async_trait;
use reiclaw_api::attribution::{Attributable, Role};
use reiclaw_api::tool::{Tool, ToolOutput, ToolResult};
use reiclaw_config::schema::{
    EstopConfig, FileDownloadConfig, FirecrawlConfig, McpServerConfig, McpTransport, PipelineConfig,
};
use reiclaw_policy::SecurityPolicy;
use reiclaw_policy::estop::EstopState;
use reiclaw_tools::file_download::FileDownloadTool;
use reiclaw_tools::http_request::HttpRequestTool;
use reiclaw_tools::mcp_client::McpRegistry;
use reiclaw_tools::pipeline::PipelineTool;
use reiclaw_tools::web_fetch::WebFetchTool;
use reiclaw_tools::web_search_tool::WebSearchTool;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write_state(path: &std::path::Path, state: &EstopState) {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(state).unwrap()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

struct CountingTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

impl Attributable for CountingTool {
    fn role(&self) -> Role {
        Role::System
    }

    fn alias(&self) -> &str {
        self.name
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "counting test tool"
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

#[tokio::test]
async fn outbound_tools_and_pipeline_and_mcp_enforce_live_estop() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("estop-state.json");
    let cfg = EstopConfig {
        enabled: true,
        state_file: state_path.to_string_lossy().to_string(),
        require_otp_to_resume: false,
    };
    let security = Arc::new(
        SecurityPolicy {
            workspace_dir: dir.path().to_path_buf(),
            ..SecurityPolicy::default()
        }
        .with_estop(&cfg, dir.path()),
    );

    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(0)
        .mount(&server)
        .await;

    let server_url = server.uri();
    let parsed = reqwest::Url::parse(&server_url).unwrap();
    let host = parsed.host_str().unwrap().to_string();

    // Engage blocked_domains matching the local test host (127.0.0.1).
    write_state(
        &state_path,
        &EstopState {
            kill_all: false,
            network_kill: false,
            blocked_domains: vec![host.clone()],
            frozen_tools: vec!["inner_frozen".into()],
            updated_at: None,
        },
    );

    let http_tool = HttpRequestTool::new(
        Arc::clone(&security),
        vec![host.clone()],
        1024,
        5,
        true,
        vec![host.clone()],
    )
    .unwrap();
    let http_res = http_tool
        .execute(json!({ "url": format!("{server_url}/test"), "method": "GET" }))
        .await
        .unwrap();
    assert!(!http_res.success);

    let fetch_tool = WebFetchTool::new(
        Arc::clone(&security),
        vec![host.clone()],
        vec![],
        1024,
        5,
        FirecrawlConfig::default(),
        vec![host.clone()],
    )
    .unwrap();
    let fetch_res = fetch_tool
        .execute(json!({ "url": format!("{server_url}/page") }))
        .await
        .unwrap();
    assert!(!fetch_res.success);

    let dl_tool = FileDownloadTool::new(
        Arc::clone(&security),
        FileDownloadConfig {
            url: Some(format!("{server_url}/doc")),
            ..FileDownloadConfig::default()
        },
    );
    let dl_res = dl_tool
        .execute(json!({ "document_id": "1", "dest_path": "out.txt" }))
        .await
        .unwrap();
    assert!(!dl_res.success);

    // PipelineTool blocks frozen nested tool before calling execute().
    let step_calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(CountingTool {
        name: "inner_frozen",
        calls: Arc::clone(&step_calls),
    }) as Arc<dyn Tool>;
    let pipeline = PipelineTool::new(
        PipelineConfig {
            enabled: true,
            allowed_tools: vec!["inner_frozen".into()],
            ..PipelineConfig::default()
        },
        vec![inner],
    )
    .with_security(Arc::clone(&security));
    let pipe_res = pipeline
        .execute(json!({
            "steps": [{ "tool": "inner_frozen", "args": {} }]
        }))
        .await
        .unwrap();
    assert!(!pipe_res.success);
    assert_eq!(step_calls.load(Ordering::SeqCst), 0);

    // Engage network_kill: WebSearchTool and MCP HTTP/Stdio refuse before any outbound call.
    write_state(
        &state_path,
        &EstopState {
            kill_all: false,
            network_kill: true,
            blocked_domains: Vec::new(),
            frozen_tools: Vec::new(),
            updated_at: None,
        },
    );

    let search_tool = WebSearchTool::new("duckduckgo".into(), None, None, 3, 5)
        .with_security(Arc::clone(&security));
    let search_res = search_tool
        .execute(json!({ "query": "rust" }))
        .await
        .unwrap();
    assert!(!search_res.success);

    let mcp_reg = McpRegistry::connect_all_with_estop(
        &[McpServerConfig {
            name: "http-srv".into(),
            transport: McpTransport::Http,
            url: Some(format!("{server_url}/mcp")),
            ..McpServerConfig::default()
        }],
        security.estop_handle().cloned(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_reg.server_count(), 0);
}
