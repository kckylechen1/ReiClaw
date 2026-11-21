//! Runtime-integrated Tachi delegation wiring and structural source-scan tests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use reiclaw_config::tachi::TachiConfig;
use serde_json::{Value, json};

use reiclaw_tools::tachi_delegation::MAX_TASK_CONTEXT_BRIEF_BYTES;
use reiclaw_tools::tachi_staff::{
    MIRROR_EXECUTION_ORIGIN, MIRROR_HARNESS, MIRROR_LIFECYCLE_OWNER, RunState, RunStatus,
    TACHI_AGENT_EVAL_TOOL, TACHI_STAFF_TOOL, TACHI_TASK_TOOL, mirror_observe_args,
    mirror_register_args, mirror_terminal_outcome,
};

// ─────────────────────────────────────────────────────────────────────────
// Scripted MCP server
// ─────────────────────────────────────────────────────────────────────────

enum Reply {
    Ok(Value),
    ToolError(String),
    ToolResult(Value),
    Http(StatusCode),
}

struct Call {
    tool: String,
    args: Value,
}

type Script = Box<dyn Fn(&str, &Value) -> Reply + Send + Sync>;

struct Fake {
    script: Script,
    calls: Mutex<Vec<Call>>,
    sessions_opened: AtomicUsize,
    expire_next_call: AtomicBool,
    fail_next_initialize: AtomicBool,
}

impl Fake {
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .expect("calls lock")
            .iter()
            .map(|call| (call.tool.clone(), call.args.clone()))
            .collect()
    }

    /// Staff-only calls. Seam A mirror traffic uses `tachi_agent_eval` and is
    /// excluded so existing Staff binding/control assertions stay stable.
    fn staff_calls(&self) -> Vec<(String, Value)> {
        self.calls()
            .into_iter()
            .filter(|(tool, _)| tool == TACHI_STAFF_TOOL)
            .collect()
    }

    fn mirror_calls(&self) -> Vec<(String, Value)> {
        self.calls()
            .into_iter()
            .filter(|(tool, _)| tool == TACHI_AGENT_EVAL_TOOL)
            .collect()
    }
}

async fn handle(
    State(fake): State<Arc<Fake>>,
    _headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").cloned();
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => {
            let session = fake.sessions_opened.fetch_add(1, Ordering::SeqCst) + 1;
            if fake.fail_next_initialize.swap(false, Ordering::SeqCst) {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            let mut response = Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake-tachi", "version": "0" }
                }
            }))
            .into_response();
            response.headers_mut().insert(
                "Mcp-Session-Id",
                format!("session-{session}").parse().expect("header value"),
            );
            response
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/call" => {
            if fake.expire_next_call.swap(false, Ordering::SeqCst) {
                return StatusCode::NOT_FOUND.into_response();
            }
            let params = body.get("params").cloned().unwrap_or(Value::Null);
            let tool = params["name"].as_str().unwrap_or("").to_string();
            let args = params["arguments"].clone();
            let reply = (fake.script)(&tool, &args);
            fake.calls
                .lock()
                .expect("calls lock")
                .push(Call { tool, args });
            let text_result = |text: String, is_error: bool| {
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "isError": is_error
                    }
                }))
                .into_response()
            };
            match reply {
                Reply::Ok(value) => text_result(value.to_string(), false),
                Reply::ToolError(text) => text_result(text, true),
                Reply::ToolResult(result) => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result
                }))
                .into_response(),
                Reply::Http(status) => status.into_response(),
            }
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

fn default_mirror_fixture(args: &Value) -> Reply {
    match args["action"].as_str() {
        Some("get") => Reply::ToolError(format!(
            "no mirror eval run found for eval_run_id=None native_child_id=Some({:?})",
            args.get("native_child_id")
        )),
        Some("register") => {
            let native = args["native_child_id"].as_str().unwrap_or("missing");
            Reply::Ok(json!({
                "eval_run_id": "00000000-0000-4000-8000-000000000001",
                "created_at": "2026-10-29T00:00:00Z",
                "native_child_id": native,
                "execution_origin": args.get("execution_origin").cloned().unwrap_or(Value::Null),
                "lifecycle_owner": args.get("lifecycle_owner").cloned().unwrap_or(Value::Null),
                "harness": args.get("harness").cloned().unwrap_or(Value::Null),
                "frozen_contract_ref": args.get("frozen_contract_ref").cloned().unwrap_or(Value::Null),
                "requested_role": null,
                "requested_task_type": null
            }))
        }
        Some("observe") => Reply::Ok(json!({
            "observation_id": "00000000-0000-4000-8000-000000000002",
            "native_child_id": args.get("native_child_id").cloned().unwrap_or(Value::Null),
            "terminal_outcome": args.get("terminal_outcome").cloned().unwrap_or(Value::Null),
            "result_ref": args.get("result_ref").cloned().unwrap_or(Value::Null),
        })),
        other => Reply::ToolError(format!("unexpected default mirror action {other:?}")),
    }
}

async fn serve(
    script: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static,
) -> (Arc<Fake>, String) {
    serve_inner(script, true).await
}

/// Like [`serve`], but the script also owns `tachi_agent_eval` replies.
async fn serve_mirror_script(
    script: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static,
) -> (Arc<Fake>, String) {
    serve_inner(script, false).await
}

async fn serve_inner(
    script: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static,
    default_mirror: bool,
) -> (Arc<Fake>, String) {
    let script = move |tool: &str, args: &Value| {
        if default_mirror && tool == TACHI_AGENT_EVAL_TOOL {
            return default_mirror_fixture(args);
        }
        script(tool, args)
    };
    let fake = Arc::new(Fake {
        script: Box::new(script),
        calls: Mutex::new(Vec::new()),
        sessions_opened: AtomicUsize::new(0),
        expire_next_call: AtomicBool::new(false),
        fail_next_initialize: AtomicBool::new(false),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake tachi");
    let addr = listener.local_addr().expect("fake tachi addr");
    let app = axum::Router::new()
        .route("/mcp", post(handle))
        .with_state(fake.clone());
    reiclaw_log::spawn!(async move {
        axum::serve(listener, app).await.expect("fake tachi serves");
    });
    (fake, format!("http://{addr}/mcp"))
}

fn config(endpoint: &str) -> TachiConfig {
    TachiConfig {
        enabled: true,
        endpoint: endpoint.to_string(),
        project: Some("zeroclaw".to_string()),
        harnesses: HashMap::from([
            ("codex".to_string(), "codex_55_review".to_string()),
            ("claude".to_string(), "claude_plan".to_string()),
        ]),
        ..TachiConfig::default()
    }
}

fn working_receipt(id: &str) -> Value {
    json!({
        "dispatch_id": id,
        "state": "TASK_STATE_WORKING",
        "run_dir": format!("/home/owner/.tachi/runs/{id}"),
        "status": "completed",
        "action": "start",
        "selected_profile": "codex_55_review",
        "a_future_field": { "nested": true }
    })
}

fn production_config(temp: &tempfile::TempDir, endpoint: &str) -> reiclaw_config::schema::Config {
    use reiclaw_config::schema::{AliasedAgentConfig, RiskProfileConfig};
    let mut cfg = reiclaw_config::schema::Config {
        data_dir: temp.path().join("data"),
        tachi: config(endpoint),
        composition: Some(reiclaw_config::composition::Composition::Minimal),
        ..Default::default()
    };
    cfg.agents.insert(
        "home".into(),
        AliasedAgentConfig {
            risk_profile: "delegate".into(),
            ..Default::default()
        },
    );
    cfg.risk_profiles.insert(
        "delegate".into(),
        RiskProfileConfig {
            level: reiclaw_config::autonomy::AutonomyLevel::Full,
            sandbox_enabled: Some(false),
            ..Default::default()
        },
    );
    cfg
}

fn production_tools(
    cfg: &reiclaw_config::schema::Config,
    live: Arc<parking_lot::RwLock<reiclaw_config::schema::Config>>,
) -> Vec<Box<dyn reiclaw_api::tool::Tool>> {
    let security = Arc::new(reiclaw_policy::SecurityPolicy::for_agent(cfg, "home").unwrap());
    reiclaw_runtime::tools::all_tools_with_runtime(
        Arc::new(cfg.clone()),
        &security,
        cfg.risk_profile_for_agent("home").unwrap(),
        "home",
        Arc::new(reiclaw_runtime::platform::NativeRuntime::new()),
        Arc::new(reiclaw_memory::NoneMemory::new("none")),
        &cfg.http_request,
        &cfg.web_fetch,
        &cfg.agents,
        None,
        cfg,
        None,
        Some(live),
        None,
        None,
    )
    .tools
}

async fn invoke(
    tools: &[Box<dyn reiclaw_api::tool::Tool>],
    name: &str,
    args: Value,
) -> reiclaw_api::tool::ToolResult {
    tools
        .iter()
        .find(|tool| tool.name() == name)
        .expect("registered delegation tool")
        .execute(args)
        .await
        .unwrap()
}

fn start_args(id: &str) -> Value {
    json!({"request_id":id,"harness":"codex","task":"Review the Codex adapter on GitHub; compare these together", "staffing_reason":"explicit_user_request"})
}

#[test]
fn production_registry_omits_disabled_tachi_tools() {
    for enabled in [None, Some(false), Some(true)] {
        let temp = tempfile::TempDir::new().unwrap();
        let mut cfg = production_config(&temp, "http://127.0.0.1:6919/mcp");
        cfg.tachi = Default::default();
        if let Some(enabled) = enabled {
            cfg.tachi.enabled = enabled;
        }
        let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
        let tools = production_tools(&cfg, live);
        for name in [
            "tachi_start",
            "tachi_status",
            "tachi_result",
            "tachi_cancel",
            "tachi_watch",
        ] {
            assert_eq!(
                tools.iter().any(|tool| tool.name() == name),
                enabled == Some(true),
                "{name}: enabled={enabled:?}"
            );
        }
    }
}

#[tokio::test]
async fn production_content_block_diagnostics_preserve_start_status_result_and_watch() {
    let status_reads = Arc::new(AtomicUsize::new(0));
    let reads = status_reads.clone();
    let (fake, endpoint) = serve(move |tool, args| {
        let payload = match (tool, args["action"].as_str()) {
            (TACHI_STAFF_TOOL, Some("start")) => working_receipt("d-blocks"),
            (TACHI_STAFF_TOOL, Some("status")) => {
                let terminal = reads.fetch_add(1, Ordering::SeqCst) >= 2;
                json!({
                    "dispatch_id":"d-blocks",
                    "state":if terminal {"TASK_STATE_COMPLETED"} else {"TASK_STATE_WORKING"},
                    "status_revision":if terminal {5} else {4}
                })
            }
            (TACHI_STAFF_TOOL, Some("result")) => json!({
                "dispatch_id":"d-blocks", "state":"TASK_STATE_WORKING", "status_revision":4,
                "read_projection":{"execution_state":"orphaned", "outcome_state":"unknown"},
                "result":{"body":"Worker report ghp_fixtureOnly123", "truncated":false}
            }),
            _ => panic!("unexpected call {tool} {args}"),
        };
        Reply::ToolResult(json!({"content":[
            {"type":"text", "text":payload.to_string()},
            {"type":"text", "text":"Soft stuck warning: repeated call"},
            {"type":"text", "text":json!({
                "dispatch_id":"wrong", "state":"TASK_STATE_COMPLETED", "status_revision":99,
                "result":{"body":"untrusted replacement", "truncated":false}
            }).to_string()}
        ], "isError":false}))
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let started = invoke(&tools, "tachi_start", start_args("req-blocks")).await;
    assert!(started.success, "{:?}", started.error);
    assert_eq!(
        started.output.data().unwrap()["receipt"]["dispatch_id"],
        "d-blocks"
    );
    let replay = invoke(&tools, "tachi_start", start_args("req-blocks")).await;
    assert!(replay.success, "{:?}", replay.error);
    assert_eq!(replay.output.data().unwrap()["replayed"], true);
    for _ in 0..2 {
        let status = invoke(&tools, "tachi_status", json!({"request_id":"req-blocks"})).await;
        assert!(status.success, "{:?}", status.error);
        assert_eq!(
            status.output.data().unwrap()["status"]["state"],
            "TASK_STATE_WORKING"
        );
        assert_eq!(
            status.output.data().unwrap()["status"]["status_revision"],
            4
        );
    }
    let result = invoke(&tools, "tachi_result", json!({"request_id":"req-blocks"})).await;
    assert!(result.success, "{:?}", result.error);
    let report = result.output.data().unwrap();
    assert_eq!(report["result"]["state"], "TASK_STATE_WORKING");
    assert_eq!(report["result"]["status"]["status_revision"], 4);
    assert_eq!(
        report["result"]["status"]["read_projection"]["execution_state"],
        "orphaned"
    );
    assert_eq!(report["result"]["body"], "Worker report [REDACTED]");
    for key in ["task_state", "state_basis", "managed_run"] {
        assert_eq!(report["result"].get(key), Some(&Value::Null));
    }
    assert_eq!(report["accepted_by_body"], false);
    assert_eq!(report["trust"], "untrusted_external_report");
    let watch = invoke(
        &tools,
        "tachi_watch",
        json!({"request_id":"req-blocks", "max_wait_secs":1}),
    )
    .await;
    assert!(watch.success, "{:?}", watch.error);
    assert_eq!(watch.output.data().unwrap()["terminal"], true);
    assert_eq!(watch.output.data().unwrap()["status"]["status_revision"], 5);
    assert_eq!(status_reads.load(Ordering::SeqCst), 3);
    let calls = fake.staff_calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls
            .iter()
            .filter(|(_, args)| args["action"] == "start")
            .count(),
        1
    );
    assert!(
        calls
            .iter()
            .all(|(tool, args)| tool == TACHI_STAFF_TOOL && args["format"] == "json")
    );
    assert!(
        !fake.mirror_calls().is_empty(),
        "Seam A mirror must attempt register/observe beside Staff"
    );
}

#[tokio::test]
async fn production_result_refusal_from_an_older_peer_has_no_task_fallback() {
    let (fake, endpoint) = serve(|tool, args| match (tool, args["action"].as_str()) {
        (TACHI_STAFF_TOOL, Some("start")) => Reply::Ok(working_receipt("d-old-peer")),
        (TACHI_STAFF_TOOL, Some("result")) => {
            Reply::ToolError("tachi_staff: unsupported action 'result'".into())
        }
        _ => panic!("old-peer refusal must not fall back: {tool} {args}"),
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let started = invoke(&tools, "tachi_start", start_args("old-peer")).await;
    assert!(started.success);
    let result = invoke(&tools, "tachi_result", json!({"request_id":"old-peer"})).await;
    assert!(!result.success);
    assert_eq!(result.output.data().unwrap()["code"], "refused");
    let calls = fake.staff_calls();
    assert_eq!(
        calls.len(),
        2,
        "one start and one refused result, no fallback"
    );
    assert_eq!(calls[0].0, TACHI_STAFF_TOOL);
    assert_eq!(calls[0].1["action"], "start");
    assert_eq!(
        calls[1],
        (
            TACHI_STAFF_TOOL.into(),
            json!({"action":"result", "format":"json", "dispatch_id":"d-old-peer"})
        )
    );
    assert!(fake.calls().iter().all(|(tool, _)| tool != TACHI_TASK_TOOL));
}

#[tokio::test]
async fn production_content_block_missing_primary_keeps_start_unresolved() {
    let (fake, endpoint) = serve(|_, _| {
        Reply::ToolResult(json!({"content":[
            {"type":"text"},
            {"type":"text", "text":working_receipt("must-not-bind").to_string()}
        ], "isError":false}))
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let failed = invoke(&tools, "tachi_start", start_args("req-no-primary")).await;
    assert!(!failed.success);
    assert_eq!(failed.output.data().unwrap()["code"], "submission_unknown");
    drop(tools);
    let tools = production_tools(&cfg, live);
    let replay = invoke(&tools, "tachi_start", start_args("req-no-primary")).await;
    assert!(!replay.success);
    assert_eq!(
        replay.output.data().unwrap()["code"],
        "submission_unresolved"
    );
    let status = invoke(
        &tools,
        "tachi_status",
        json!({"request_id":"req-no-primary"}),
    )
    .await;
    assert!(!status.success);
    assert_eq!(
        status.output.data().unwrap()["code"],
        "submission_unresolved"
    );
    assert_eq!(fake.calls().len(), 1, "no receipt salvage or second start");
    let db = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let claim: (i64, Option<String>) = db
        .query_row(
            "SELECT COUNT(*), MAX(dispatch_id) FROM session_delegations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        claim,
        (1, None),
        "ambiguous transmission retains its pending claim"
    );
}

#[tokio::test]
async fn production_registry_starts_once_and_reads_controls_through_same_request_binding() {
    let (fake, endpoint) = serve(|tool, args| match (tool, args["action"].as_str()) {
        (TACHI_STAFF_TOOL, Some("start")) => Reply::Ok(working_receipt("d-prod")),
        (TACHI_STAFF_TOOL, Some("status")) => Reply::Ok(json!({"dispatch_id":"d-prod", "state":"TASK_STATE_COMPLETED", "status_revision":4})),
        (TACHI_STAFF_TOOL, Some("cancel")) => {
            assert_eq!(args["expected_status_revision"], 4);
            Reply::Ok(json!({"receipt":"cancellation_requested", "state":"TASK_STATE_WORKING"}))
        }
        (TACHI_STAFF_TOOL, Some("result")) => Reply::Ok(json!({"dispatch_id":"d-prod", "state":"TASK_STATE_COMPLETED", "status_revision":4, "result":{"body":"Worker report", "truncated":false}})),
        _ => panic!("unexpected call {tool} {args}")
    }).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let protected_dir = cfg.agent_workspace_dir("home");
    std::fs::create_dir_all(&protected_dir).unwrap();
    std::fs::write(
        protected_dir.join("SOUL.md"),
        "protected-soul-fixture-bytes",
    )
    .unwrap();
    std::fs::write(
        protected_dir.join("USER.md"),
        "protected-user-model-fixture-bytes",
    )
    .unwrap();
    // Seed the canonical stores as well as legacy files: delegation must not
    // inherit either the active Soul or the owner-approved User Model.
    reiclaw_memory::companion::SoulProfileStore::shared(&cfg.data_dir)
        .unwrap()
        .ensure_seeded("home", "protected-soul-fixture-bytes", 1)
        .unwrap();
    reiclaw_memory::companion::UserModelStore::shared(&cfg.data_dir)
        .unwrap()
        .record_owner_statement(
            reiclaw_memory::companion::UserModelKind::Preference,
            "protected-user-model-fixture-bytes",
            "privacy.fixture",
            "global",
            1,
        )
        .unwrap();
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let started = invoke(&tools, "tachi_start", start_args("req-prod")).await;
    assert!(started.success, "{:?}", started.error);
    assert_eq!(
        started.output.data().unwrap()["receipt"]["dispatch_id"],
        "d-prod"
    );
    drop(tools);
    let tools = production_tools(&cfg, live);
    let replay = invoke(&tools, "tachi_start", start_args("req-prod")).await;
    assert_eq!(replay.output.data().unwrap()["replayed"], true);
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|(_, args)| args["action"] == "start")
            .count(),
        1
    );
    let status = invoke(&tools, "tachi_status", json!({"request_id":"req-prod"})).await;
    assert_eq!(
        status.output.data().unwrap()["status"]["status_revision"],
        4
    );
    let result = invoke(&tools, "tachi_result", json!({"request_id":"req-prod"})).await;
    assert_eq!(result.output.data().unwrap()["accepted_by_body"], false);
    assert_eq!(
        result.output.data().unwrap()["trust"],
        "untrusted_external_report"
    );
    let watch = invoke(
        &tools,
        "tachi_watch",
        json!({"request_id":"req-prod","max_wait_secs":1}),
    )
    .await;
    assert_eq!(watch.output.data().unwrap()["terminal"], true);
    let cancel = invoke(
        &tools,
        "tachi_cancel",
        json!({"request_id":"req-prod","expected_status_revision":4}),
    )
    .await;
    assert_eq!(
        cancel.output.data().unwrap()["receipt"]["outcome"],
        "requested"
    );
    let starts = fake.calls();
    assert!(starts[0].1.to_string().contains("Codex adapter on GitHub"));
    for (tool, args) in starts {
        let wire = args.to_string();
        for protected in [
            "protected-soul-fixture-bytes",
            "protected-user-model-fixture-bytes",
        ] {
            assert!(!wire.contains(protected));
        }
        if tool == TACHI_STAFF_TOOL && args["action"] == "start" {
            for denied in [
                "worker",
                "command",
                "cwd",
                "credentials",
                "sandbox",
                "allowed_tools",
                "soul",
                "user_model",
                "history",
            ] {
                assert!(args.get(denied).is_none(), "{denied}");
            }
        }
    }
}

#[tokio::test]
async fn production_unsent_initialize_failure_releases_claim_and_same_id_can_retry() {
    let (fake, endpoint) = serve(|_, _| Reply::Ok(working_receipt("d-after-outage"))).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    fake.fail_next_initialize.store(true, Ordering::SeqCst);
    let failed = invoke(&tools, "tachi_start", start_args("same-id")).await;
    assert_eq!(failed.output.data().unwrap()["code"], "unavailable");
    assert!(fake.calls().is_empty());
    let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_delegations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0, "proven unsent start must not strand its request");

    let retry = invoke(&tools, "tachi_start", start_args("same-id")).await;
    assert_eq!(
        retry.output.data().unwrap()["receipt"]["dispatch_id"],
        "d-after-outage"
    );
    assert_eq!(fake.staff_calls().len(), 1);
    let replay = invoke(&tools, "tachi_start", start_args("same-id")).await;
    assert_eq!(replay.output.data().unwrap()["replayed"], true);
    assert_eq!(
        fake.staff_calls().len(),
        1,
        "replay must not start another Staff worker"
    );
    assert!(
        fake.mirror_calls()
            .iter()
            .any(|(_, args)| args["action"] == "get" || args["action"] == "register"),
        "bound replay must attempt mirror register repair"
    );
}

#[tokio::test]
async fn production_task_and_refs_reject_execution_and_private_content_before_claim() {
    let (fake, endpoint) = serve(|_, _| panic!("forbidden content must never reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    for text in [
        "run in /Users/example/worktrees/change",
        "bash build-script",
        "use cwd chosen by this prompt",
        "run using tmux",
        "reach the host via SSH",
        "skip the sandbox",
        "pass --full-auto",
        "api_key=fixture-only",
        "include the private-dyad identity",
    ] {
        for field in ["task", "issue_ref", "pr_ref", "flow_id"] {
            // Each case has a fresh action tracker so rate limiting cannot
            // mask a broken admission scanner on later cases.
            let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
            let mut args = start_args("forbidden");
            args[field] = json!(text);
            let result = invoke(&tools, "tachi_start", args).await;
            assert_eq!(
                result.output.data().unwrap()["code"],
                "forbidden_content",
                "{field}: {text}"
            );
        }
    }
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let result = invoke(&tools, "tachi_start", start_args("ghp_fixture-only")).await;
    assert_eq!(result.output.data().unwrap()["code"], "forbidden_content");
    let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_delegations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_unknown_submit_survives_registry_restart_and_is_never_reissued() {
    let (fake, endpoint) = serve(|_, _| Reply::Http(StatusCode::NOT_FOUND)).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let first = invoke(&tools, "tachi_start", start_args("lost-response")).await;
    assert_eq!(first.output.data().unwrap()["code"], "submission_unknown");
    drop(tools);
    let tools = production_tools(&cfg, live);
    let replay = invoke(&tools, "tachi_start", start_args("lost-response")).await;
    assert_eq!(
        replay.output.data().unwrap()["code"],
        "submission_unresolved"
    );
    assert_eq!(fake.calls().len(), 1);
    let read = invoke(
        &tools,
        "tachi_status",
        json!({"request_id":"lost-response"}),
    )
    .await;
    assert_eq!(read.output.data().unwrap()["code"], "submission_unresolved");
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test]
async fn production_unresolved_claim_blocks_live_route_changes_and_registry_restart() {
    assert_route_changes_keep_one_claim(false, &["endpoint", "identity", "project"]).await;
}

#[tokio::test]
async fn production_bound_claim_blocks_wrong_route_reads_controls_and_resubmission() {
    assert_route_changes_keep_one_claim(true, &["endpoint", "identity", "project"]).await;
}

#[tokio::test]
async fn production_live_storage_root_change_refuses_before_claim_or_transport() {
    for accepted in [false, true] {
        assert_route_changes_keep_one_claim(accepted, &["data_dir"]).await;
    }
}

async fn assert_route_changes_keep_one_claim(accepted: bool, changes: &[&str]) {
    for &change in changes {
        let (old, endpoint) = serve(move |_, args| match args["action"].as_str() {
            Some("start") if accepted => Reply::Ok(working_receipt("d-route-original")),
            Some("start") => Reply::Http(StatusCode::NOT_FOUND),
            Some("status") => Reply::Ok(json!({"dispatch_id":"d-route-original", "state":"TASK_STATE_COMPLETED", "status_revision":4})),
            _ => panic!("unexpected original-route call"),
        }).await;
        let (other, other_endpoint) =
            serve(|_, _| Reply::Ok(working_receipt("wrong-daemon"))).await;
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = production_config(&temp, &endpoint);
        let foreign_root = temp.path().join("must-not-adopt-live-root");
        let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
        let mut tools = production_tools(&cfg, live.clone());
        let first = invoke(&tools, "tachi_start", start_args("route-stable-id")).await;
        if accepted {
            assert_eq!(
                first.output.data().unwrap()["receipt"]["dispatch_id"],
                "d-route-original"
            );
        } else {
            assert_eq!(first.output.data().unwrap()["code"], "submission_unknown");
        }
        assert_eq!(old.staff_calls().len(), 1);
        {
            let mut config = live.write();
            match change {
                "endpoint" => config.tachi.endpoint = other_endpoint,
                "identity" => config.tachi.agent_identity = Some("zeroclaw:other-route".into()),
                "project" => config.tachi.project = Some("other-project".into()),
                "data_dir" => config.data_dir = foreign_root.clone(),
                _ => panic!("unknown fixture transition"),
            }
        }
        for recreated in [false, true] {
            if recreated {
                drop(tools);
                tools = production_tools(&cfg, live.clone());
            }
            let start = invoke(&tools, "tachi_start", start_args("route-stable-id")).await;
            assert!(!start.success, "{change}/{recreated}: {start:?}");
            assert_eq!(
                start.output.data().unwrap()["code"],
                if change == "data_dir" {
                    "storage_root_changed"
                } else {
                    "request_conflict"
                }
            );
            for name in [
                "tachi_status",
                "tachi_result",
                "tachi_watch",
                "tachi_cancel",
            ] {
                let args = match name {
                    "tachi_watch" => json!({"request_id":"route-stable-id","max_wait_secs":1}),
                    "tachi_cancel" => {
                        json!({"request_id":"route-stable-id","expected_status_revision":4})
                    }
                    _ => json!({"request_id":"route-stable-id"}),
                };
                let result = invoke(&tools, name, args).await;
                assert!(!result.success, "{change}/{recreated}/{name}: {result:?}");
                assert_eq!(
                    result.output.data().unwrap()["code"],
                    if change == "data_dir" {
                        "storage_root_changed"
                    } else {
                        "route_conflict"
                    }
                );
            }
            assert_eq!(
                old.staff_calls().len(),
                1,
                "no old Staff reference forwarded after {change}"
            );
            assert!(
                other.calls().is_empty(),
                "no duplicate submission to changed route"
            );
            assert_eq!(old.sessions_opened.load(Ordering::SeqCst), 1);
            assert_eq!(other.sessions_opened.load(Ordering::SeqCst), 0);
            assert!(
                !foreign_root.exists(),
                "live ledger switch must fail before DB creation"
            );
        }
        *live.write() = cfg.clone();
        let restored = invoke(&tools, "tachi_start", start_args("route-stable-id")).await;
        if accepted {
            assert_eq!(restored.output.data().unwrap()["replayed"], true);
            assert_eq!(
                restored.output.data().unwrap()["dispatch_id"],
                "d-route-original"
            );
            let status = invoke(
                &tools,
                "tachi_status",
                json!({"request_id":"route-stable-id"}),
            )
            .await;
            assert_eq!(
                status.output.data().unwrap()["status"]["dispatch_id"],
                "d-route-original"
            );
            assert_eq!(
                old.staff_calls().len(),
                2,
                "only original-route status positive control"
            );
        } else {
            assert_eq!(
                restored.output.data().unwrap()["code"],
                "submission_unresolved"
            );
            let status = invoke(
                &tools,
                "tachi_status",
                json!({"request_id":"route-stable-id"}),
            )
            .await;
            assert_eq!(
                status.output.data().unwrap()["code"],
                "submission_unresolved"
            );
            assert_eq!(old.staff_calls().len(), 1);
        }
        assert!(other.calls().is_empty());
        let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM session_delegations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1, "route changes cannot create a second claim");
        let alias: String = conn
            .query_row("SELECT agent_alias FROM session_delegations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            alias, "home",
            "stable local agent identity owns request uniqueness"
        );
    }
}

#[tokio::test]
async fn production_closed_unknown_harness_and_raw_authority_have_zero_external_side_effects() {
    let (fake, endpoint) = serve(|_, _| panic!("must not reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let mut unknown = start_args("unknown");
    unknown["harness"] = json!("unregistered");
    assert_eq!(
        invoke(&tools, "tachi_start", unknown)
            .await
            .output
            .data()
            .unwrap()["code"],
        "unknown_harness"
    );
    for field in [
        "command",
        "cwd",
        "credentials",
        "worker",
        "allowed_tools",
        "sandbox",
        "soul",
        "user_model",
        "history",
    ] {
        let mut raw = start_args("hostile");
        raw[field] = json!("untrusted");
        assert_eq!(
            invoke(&tools, "tachi_start", raw)
                .await
                .output
                .data()
                .unwrap()["code"],
            "invalid_arguments"
        );
    }
    let store = reiclaw_infra::session_sqlite::SqliteSessionBackend::new(&cfg.data_dir).unwrap();
    // Other registered session tools may have opened this DB; admission must
    // leave no claim rather than pretending registry construction is I/O-free.
    let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM session_delegations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    drop(store);
    live.write().tachi.enabled = false;
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("closed"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "unavailable"
    );
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn production_status_and_watch_preserve_orphaned_projection_without_terminal_promotion() {
    let projection = json!({
        "execution_state":"orphaned",
        "control_state":"unavailable",
        "outcome_state":"unknown",
        "controller_epoch_id":"previous-epoch",
        "current_controller_epoch_id":"current-epoch",
        "reconciliation":{"verdict":"orphaned"},
        "artifacts_available":{"result.md":false}
    });
    let expected = projection.clone();
    let (_, endpoint) = serve(move |_, args| match args["action"].as_str() {
        Some("start") => Reply::Ok(working_receipt("d-orphaned")),
        Some("status") => Reply::Ok(json!({
            "dispatch_id":"d-orphaned",
            "state":"TASK_STATE_WORKING",
            "status_revision":4,
            "read_projection":projection
        })),
        _ => panic!("unexpected call"),
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let started = invoke(&tools, "tachi_start", start_args("orphaned")).await;
    assert_eq!(started.output.data().unwrap()["accepted"], true);
    for name in ["tachi_status", "tachi_watch"] {
        let args = if name == "tachi_watch" {
            json!({"request_id":"orphaned","max_wait_secs":1})
        } else {
            json!({"request_id":"orphaned"})
        };
        let result = invoke(&tools, name, args).await;
        let data = result.output.data().unwrap();
        assert_eq!(data["status"]["state"], "TASK_STATE_WORKING");
        assert_eq!(data["status"]["read_projection"], expected);
        if name == "tachi_watch" {
            assert_eq!(data["terminal"], false);
        }
    }
}

#[tokio::test]
async fn live_agent_construction_rechecks_delegation_policy_after_reload() {
    use reiclaw_config::multi_agent::MemoryBackendKind;
    use reiclaw_config::schema::{ModelProviderConfig, OllamaModelProviderConfig};

    let (fake, endpoint) = serve(|_, _| panic!("revoked permission must not reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let mut cfg = production_config(&temp, &endpoint);
    cfg.config_path = temp.path().join("config.toml");
    cfg.providers.models.ollama.insert(
        "offline".into(),
        OllamaModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("fixture-model".into()),
                ..Default::default()
            },
        },
    );
    let agent_cfg = cfg.agents.get_mut("home").unwrap();
    agent_cfg.model_provider = "ollama.offline".into();
    agent_cfg.memory.backend = MemoryBackendKind::None;
    let live = Arc::new(parking_lot::RwLock::new(cfg));
    let agent =
        reiclaw_runtime::agent::Agent::from_live_config_with_session_cwd_and_mcp_backchannel(
            live.clone(),
            "home",
            None,
            false,
            true,
        )
        .await
        .unwrap();
    assert!(agent.tool_names().contains(&"tachi_start"));

    live.write()
        .risk_profiles
        .get_mut("delegate")
        .unwrap()
        .level = reiclaw_config::autonomy::AutonomyLevel::ReadOnly;
    let denied = agent
        .execute_tool_for_test("tachi_start", start_args("real-agent-revoked"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(denied.output.data().unwrap()["code"], "denied");
    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_live_policy_and_profile_revocations_apply_before_submission() {
    let (fake, endpoint) = serve(|_, _| panic!("must not reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    live.write().tachi.harnesses.clear();
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("revoked-profile"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "unknown_harness"
    );
    live.write().tachi.harnesses = cfg.tachi.harnesses.clone();
    live.write()
        .risk_profiles
        .get_mut("delegate")
        .unwrap()
        .level = reiclaw_config::autonomy::AutonomyLevel::ReadOnly;
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("read-only"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "denied"
    );
    {
        let mut changed = live.write();
        changed.risk_profiles.get_mut("delegate").unwrap().level =
            reiclaw_config::autonomy::AutonomyLevel::Full;
        changed
            .risk_profiles
            .get_mut("delegate")
            .unwrap()
            .allowed_tools = Some(vec![]);
    }
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("denied"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "denied"
    );
    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

// ─────────────────────────────────────────────────────────────────────────
// Seam A Staff experience mirror (#509)
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn mirror_register_observe_on_status_result_and_watch_share_one_payload() {
    let store = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
    let observations = Arc::new(Mutex::new(Vec::<Value>::new()));
    let store_get = store.clone();
    let store_reg = store.clone();
    let obs = observations.clone();
    let (fake, endpoint) = serve_mirror_script(move |tool, args| match (tool, args["action"].as_str()) {
        (TACHI_STAFF_TOOL, Some("start")) => Reply::Ok(working_receipt("d-mirror")),
        (TACHI_STAFF_TOOL, Some("status")) | (TACHI_STAFF_TOOL, Some("result")) => {
            let status = json!({
                "dispatch_id":"d-mirror",
                "state":"TASK_STATE_COMPLETED",
                "status_revision":3,
                "updated_at":"2026-10-29T12:00:00Z",
                "result_written":true
            });
            if args["action"] == "result" {
                Reply::Ok(json!({
                    "dispatch_id":"d-mirror",
                    "state":"TASK_STATE_COMPLETED",
                    "status_revision":3,
                    "updated_at":"2026-10-29T12:00:00Z",
                    "result_written":true,
                    "result":{"body":"Worker report with costs ignored", "truncated":true}
                }))
            } else {
                Reply::Ok(status)
            }
        }
        (TACHI_AGENT_EVAL_TOOL, Some("get")) => {
            let native = args["native_child_id"].as_str().unwrap_or("").to_string();
            match store_get.lock().expect("store").get(&native).cloned() {
                Some(row) => Reply::Ok(row),
                None => Reply::ToolError(format!(
                    "no mirror eval run found for eval_run_id=None native_child_id=Some({native:?})"
                )),
            }
        }
        (TACHI_AGENT_EVAL_TOOL, Some("register")) => {
            let native = args["native_child_id"].as_str().unwrap_or("").to_string();
            let expected = mirror_register_args(&native);
            assert_eq!(args, &expected, "frozen register payload");
            let row = json!({
                "eval_run_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "created_at":"2026-10-29T12:00:00Z",
                "native_child_id":native,
                "execution_origin":MIRROR_EXECUTION_ORIGIN,
                "lifecycle_owner":MIRROR_LIFECYCLE_OWNER,
                "harness":MIRROR_HARNESS,
                "frozen_contract_ref":expected["frozen_contract_ref"],
                "observation":null,
                "adjudications":[]
            });
            store_reg
                .lock()
                .expect("store")
                .insert(native, row.clone());
            Reply::Ok(row)
        }
        (TACHI_AGENT_EVAL_TOOL, Some("observe")) => {
            obs.lock().expect("obs").push(args.clone());
            Reply::Ok(json!({
                "observation_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "native_child_id":args["native_child_id"],
                "terminal_outcome":args["terminal_outcome"]
            }))
        }
        _ => panic!("unexpected call {tool} {args}"),
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let started = invoke(&tools, "tachi_start", start_args("req-mirror")).await;
    assert!(started.success, "{:?}", started.error);
    assert_eq!(
        store.lock().expect("store").len(),
        1,
        "start_bind must register"
    );

    let status = invoke(&tools, "tachi_status", json!({"request_id":"req-mirror"})).await;
    assert!(status.success, "{:?}", status.error);
    let result = invoke(&tools, "tachi_result", json!({"request_id":"req-mirror"})).await;
    assert!(result.success, "{:?}", result.error);
    let watch = invoke(
        &tools,
        "tachi_watch",
        json!({"request_id":"req-mirror","max_wait_secs":1}),
    )
    .await;
    assert!(watch.success, "{:?}", watch.error);

    let observed = observations.lock().expect("obs").clone();
    assert_eq!(observed.len(), 3, "status/result/watch each observe once");
    let expected = mirror_observe_args("d-mirror", "success");
    for payload in &observed {
        assert_eq!(payload, &expected);
    }
    for (_, args) in fake.mirror_calls() {
        let wire = args.to_string();
        assert!(!wire.contains("Worker report"));
        assert!(!wire.contains("SOUL"));
        assert_ne!(
            args.get("execution_origin"),
            Some(&json!("host_native_subagent"))
        );
        assert_ne!(args.get("lifecycle_owner"), Some(&json!("host")));
    }
}

#[tokio::test]
async fn mirror_peer_errors_are_fail_soft_and_never_replay_staff_start() {
    let staff_starts = Arc::new(AtomicUsize::new(0));
    let starts = staff_starts.clone();
    let (fake, endpoint) =
        serve_mirror_script(move |tool, args| match (tool, args["action"].as_str()) {
            (TACHI_STAFF_TOOL, Some("start")) => {
                starts.fetch_add(1, Ordering::SeqCst);
                Reply::Ok(working_receipt("d-failsoft"))
            }
            (TACHI_STAFF_TOOL, Some("status")) => Reply::Ok(json!({
                "dispatch_id":"d-failsoft",
                "state":"TASK_STATE_FAILED",
                "status_revision":2
            })),
            (TACHI_AGENT_EVAL_TOOL, _) => Reply::ToolError("mirror peer refused".into()),
            _ => panic!("unexpected call {tool} {args}"),
        })
        .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let started = invoke(&tools, "tachi_start", start_args("req-failsoft")).await;
    assert!(
        started.success,
        "Staff start must succeed when mirror fails"
    );
    let status = invoke(&tools, "tachi_status", json!({"request_id":"req-failsoft"})).await;
    assert!(
        status.success,
        "Staff status must succeed when mirror fails"
    );
    assert_eq!(
        status.output.data().unwrap()["status"]["state"],
        "TASK_STATE_FAILED"
    );
    assert_eq!(staff_starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake.staff_calls()
            .iter()
            .filter(|(_, args)| args["action"] == "start")
            .count(),
        1
    );
    assert!(
        !fake.mirror_calls().is_empty(),
        "mirror attempts still happen under fail-soft"
    );
}

#[tokio::test]
async fn mirror_restart_after_bind_repairs_register_before_terminal_observe() {
    let store = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
    let registered = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let store_get = store.clone();
    let store_reg = store.clone();
    let reg_count = registered.clone();
    let obs_count = observed.clone();
    let (fake, endpoint) = serve_mirror_script(move |tool, args| match (tool, args["action"].as_str()) {
        (TACHI_STAFF_TOOL, Some("start")) => Reply::Ok(working_receipt("d-restart")),
        (TACHI_STAFF_TOOL, Some("status")) => Reply::Ok(json!({
            "dispatch_id":"d-restart",
            "state":"TASK_STATE_CANCELED",
            "status_revision":9
        })),
        (TACHI_AGENT_EVAL_TOOL, Some("get")) => {
            let native = args["native_child_id"].as_str().unwrap_or("").to_string();
            match store_get.lock().expect("store").get(&native).cloned() {
                Some(row) => Reply::Ok(row),
                None => Reply::ToolError(format!(
                    "no mirror eval run found for eval_run_id=None native_child_id=Some({native:?})"
                )),
            }
        }
        (TACHI_AGENT_EVAL_TOOL, Some("register")) => {
            reg_count.fetch_add(1, Ordering::SeqCst);
            let native = args["native_child_id"].as_str().unwrap_or("").to_string();
            let row = json!({
                "eval_run_id":"cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                "native_child_id":native,
                "execution_origin":MIRROR_EXECUTION_ORIGIN,
                "lifecycle_owner":MIRROR_LIFECYCLE_OWNER,
                "harness":MIRROR_HARNESS,
                "frozen_contract_ref":format!("reiclaw:staff:{native}"),
                "observation":null
            });
            store_reg
                .lock()
                .expect("store")
                .insert(native, row.clone());
            Reply::Ok(row)
        }
        (TACHI_AGENT_EVAL_TOOL, Some("observe")) => {
            obs_count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(args, &mirror_observe_args("d-restart", "aborted"));
            Reply::Ok(json!({"observation_id":"obs-1"}))
        }
        _ => panic!("unexpected call {tool} {args}"),
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let started = invoke(&tools, "tachi_start", start_args("req-restart")).await;
    assert!(started.success, "{:?}", started.error);
    assert_eq!(registered.load(Ordering::SeqCst), 1);
    // Simulate lost mirror row after bind (crash before durable ledger ack).
    store.lock().expect("store").clear();
    drop(tools);
    let tools = production_tools(&cfg, live);
    let status = invoke(&tools, "tachi_status", json!({"request_id":"req-restart"})).await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(
        registered.load(Ordering::SeqCst),
        2,
        "status must repair register"
    );
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(
        mirror_terminal_outcome(&RunStatus {
            dispatch_id: "d-restart".into(),
            state: RunState::Canceled,
            revision: Some(9),
            closure_kind: None,
            result_written: None,
            updated_at: None,
            read_projection: None,
            native: Default::default(),
        }),
        Some("aborted")
    );
    assert_eq!(
        fake.staff_calls()
            .iter()
            .filter(|(_, args)| args["action"] == "start")
            .count(),
        1
    );
}

#[tokio::test]
async fn mirror_watch_timeout_while_running_does_not_observe() {
    let observed = Arc::new(AtomicUsize::new(0));
    let obs = observed.clone();
    let (_fake, endpoint) =
        serve_mirror_script(move |tool, args| match (tool, args["action"].as_str()) {
            (TACHI_STAFF_TOOL, Some("start")) => Reply::Ok(working_receipt("d-watch")),
            (TACHI_STAFF_TOOL, Some("status")) => Reply::Ok(json!({
                "dispatch_id":"d-watch",
                "state":"TASK_STATE_WORKING",
                "status_revision":1
            })),
            (TACHI_AGENT_EVAL_TOOL, Some("get")) => Reply::ToolError(
                "no mirror eval run found for eval_run_id=None native_child_id=Some(\"d-watch\")"
                    .into(),
            ),
            (TACHI_AGENT_EVAL_TOOL, Some("register")) => Reply::Ok(json!({
                "eval_run_id":"dddddddd-dddd-4ddd-8ddd-dddddddddddd",
                "native_child_id":"d-watch",
                "execution_origin":MIRROR_EXECUTION_ORIGIN,
                "lifecycle_owner":MIRROR_LIFECYCLE_OWNER,
                "harness":MIRROR_HARNESS,
                "frozen_contract_ref":"reiclaw:staff:d-watch"
            })),
            (TACHI_AGENT_EVAL_TOOL, Some("observe")) => {
                obs.fetch_add(1, Ordering::SeqCst);
                Reply::Ok(json!({}))
            }
            _ => panic!("unexpected call {tool} {args}"),
        })
        .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    assert!(
        invoke(&tools, "tachi_start", start_args("req-watch"))
            .await
            .success
    );
    let watch = invoke(
        &tools,
        "tachi_watch",
        json!({"request_id":"req-watch","max_wait_secs":1}),
    )
    .await;
    assert!(watch.success, "{:?}", watch.error);
    assert_eq!(watch.output.data().unwrap()["terminal"], false);
    assert_eq!(observed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tachi_start_with_context_brief_redacts_secrets_excludes_soul_and_rejects_without_claim_mutation()
 {
    let (fake, endpoint) = serve(|tool, args| {
        assert_eq!(tool, TACHI_STAFF_TOOL);
        assert_eq!(args["action"], "start");
        Reply::Ok(working_receipt("d-brief"))
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    reiclaw_memory::companion::SoulProfileStore::shared(&cfg.data_dir)
        .unwrap()
        .ensure_seeded("home", "protected-soul-fixture-bytes", 1)
        .unwrap();
    reiclaw_memory::companion::UserModelStore::shared(&cfg.data_dir)
        .unwrap()
        .record_owner_statement(
            reiclaw_memory::companion::UserModelKind::Preference,
            "protected-user-model-fixture-bytes",
            "privacy.fixture",
            "global",
            1,
        )
        .unwrap();
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));

    // Oversized, credential-marked, and Soul/UserModel briefs must reject cleanly
    // BEFORE mutating session_delegations admission claims.
    let over_brief = "a".repeat(MAX_TASK_CONTEXT_BRIEF_BYTES + 1);
    for (req_id, bad_brief, expected_code) in [
        ("req-over", over_brief.as_str(), "invalid_arguments"),
        ("req-cred", "api_key=raw-secret-123", "forbidden_content"),
        ("req-soul", "Check soul.db persona", "forbidden_content"),
        ("req-um", "Read user_model.db", "forbidden_content"),
    ] {
        let mut args = start_args(req_id);
        args["context_brief"] = json!(bad_brief);
        let rejected = invoke(&tools, "tachi_start", args).await;
        assert!(!rejected.success);
        assert_eq!(rejected.output.data().unwrap()["code"], expected_code);
    }
    assert!(fake.calls().is_empty(), "rejected briefs never reach Tachi");
    let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let claim_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_delegations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(claim_count, 0, "rejected briefs must not mutate claims");

    // Valid technical brief with a scrubbable token pattern redacts cleanly and
    // omits all Soul / User Model bytes from the outbound task.
    let mut ok_args = start_args("req-brief-ok");
    ok_args["context_brief"] = json!(
        "Decision: preserve existing sessions table; callback https://example.test/hook?token=secret99; old key sk-ant-secret42."
    );
    let accepted = invoke(&tools, "tachi_start", ok_args).await;
    assert!(accepted.success, "{:?}", accepted.error);
    let sent_calls = fake.staff_calls();
    assert_eq!(sent_calls.len(), 1);
    let sent_task = sent_calls[0].1["task"].as_str().unwrap();
    assert!(sent_task.starts_with("[Task Context]\n"));
    assert!(sent_task.contains("Decision: preserve existing sessions table"));
    assert!(sent_task.contains("[REDACTED]"));
    assert!(!sent_task.contains("secret99"));
    assert!(!sent_task.contains("sk-ant-secret42"));
    assert!(!sent_task.contains("protected-soul-fixture-bytes"));
    assert!(!sent_task.contains("protected-user-model-fixture-bytes"));
}

#[test]
fn module_source_scans_hold() {
    // Row 12 structural halves + TB-23 item 4:
    // (a) the bridge client module contains no process/command execution
    //     capability (TB-20: there is nothing to fall back TO);
    // (b) it imports nothing from zeroclaw-eval;
    // (c) the local-chat/agent path does not reference the bridge (the
    //     positive control: ordinary local chat cannot depend on Tachi
    //     availability because nothing in the agent path names it).
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let sources = [
        format!("{manifest_dir}/../reiclaw-api/src/tachi_staff.rs"),
        format!("{manifest_dir}/../reiclaw-tools/src/tachi_staff.rs"),
        format!("{manifest_dir}/../reiclaw-tools/src/tachi_delegation.rs"),
        format!("{manifest_dir}/../reiclaw-tools/src/tachi_admission.rs"),
    ];
    for file in sources {
        let source =
            std::fs::read_to_string(&file).unwrap_or_else(|error| panic!("read {file}: {error}"));
        for banned in [
            "std::process",
            "tokio::process",
            "process::Command",
            "reiclaw_eval",
        ] {
            assert!(
                !source.contains(banned),
                "{file} must not contain {banned} (TB-20/TB-23)"
            );
        }
    }
    // (c) scan the agent module tree for references to the bridge.
    let agent_dir = format!("{manifest_dir}/src/agent");
    let mut scanned = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(agent_dir)];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read dir {}: {error}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                scanned += 1;
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                for forbidden in ["tachi_bridge", "tachi_staff", "tachi_delegation"] {
                    assert!(
                        !source.contains(forbidden),
                        "{} must not reference {forbidden} (TB-20 positive control)",
                        path.display()
                    );
                }
            }
        }
    }
    assert!(
        scanned > 10,
        "agent tree scan must actually cover sources ({scanned})"
    );
}
