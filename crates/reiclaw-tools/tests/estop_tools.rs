use reiclaw_api::tool::Tool;
use reiclaw_config::schema::{EstopConfig, FirecrawlConfig, McpServerConfig, McpTransport};
use reiclaw_policy::SecurityPolicy;
use reiclaw_policy::estop::EstopState;
use reiclaw_tools::http_request::HttpRequestTool;
use reiclaw_tools::mcp_client::McpRegistry;
use reiclaw_tools::web_fetch::WebFetchTool;
use reiclaw_tools::web_search_tool::WebSearchTool;
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write_state(path: &std::path::Path, state: &EstopState) {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(state).unwrap()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[tokio::test]
async fn outbound_tools_and_mcp_enforce_live_estop() {
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
            frozen_tools: Vec::new(),
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
