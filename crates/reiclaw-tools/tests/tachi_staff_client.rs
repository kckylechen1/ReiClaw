//! `tachi_staff` client tests against a scripted MCP server that speaks the
//! same streamable-HTTP JSON-RPC shape as the Tachi daemon.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use reiclaw_config::tachi::TachiConfig;
use serde_json::{Value, json};

use reiclaw_tools::tachi_delegation::{
    MAX_PROXY_CONTINUATIONS_PER_DISPATCH, MAX_TASK_CONTEXT_BRIEF_BYTES, ProxyContinuationError,
    ProxyContinuationGate, TaskBriefError, format_redacted_task_with_brief,
    validate_proxy_continuation,
};
use reiclaw_tools::tachi_staff::{
    CancelOutcome, ContinueReceipt, FinishReceipt, HEADER_AGENT_IDENTITY, HEADER_CLIENT,
    HEADER_PROFILE, HEADER_PROJECT, RunState, RunStatus, StaffReceipt, StaffRefs, StaffingReason,
    TACHI_AGENT_EVAL_TOOL, TACHI_STAFF_TOOL, TachiStaffClient, TachiStaffError,
};

/// Tachi's golden external-staffing contract (copied verbatim from
/// kckylechen1/tachi `docs/engineering/architecture/`, tachi@fbab02c).
const STAFFING_CONTRACT_FIXTURE: &str =
    include_str!("fixtures/external-staffing-contract-v1.fixture.json");

/// Every field `TachiStaffParams` accepts (tachi-params
/// `facade/orchestration.rs`, `deny_unknown_fields`). A key outside this set
/// would make Tachi refuse the whole call.
const TACHI_STAFF_PARAM_FIELDS: &[&str] = &[
    "action",
    "format",
    "dispatch_id",
    "expected_status_revision",
    "task",
    "staffing_reason",
    "profile",
    "worker",
    "project",
    "stage",
    "issue_ref",
    "pr_ref",
    "flow_id",
    "recommendation_ref",
    "declared_file_scope",
];

// ─────────────────────────────────────────────────────────────────────────
// Scripted MCP server
// ─────────────────────────────────────────────────────────────────────────

enum Reply {
    Ok(Value),
    Text(String),
    ToolError(String),
    ToolResult(Value),
    RpcError(i64, String),
    Http(StatusCode),
}

struct Call {
    headers: HashMap<String, String>,
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
}

async fn handle(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
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
            let recorded = headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        value.to_str().unwrap_or("").to_string(),
                    )
                })
                .collect();
            let reply = (fake.script)(&tool, &args);
            fake.calls.lock().expect("calls lock").push(Call {
                headers: recorded,
                tool,
                args,
            });
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
                Reply::Text(text) => text_result(text, false),
                Reply::ToolError(text) => text_result(text, true),
                Reply::ToolResult(result) => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result
                }))
                .into_response(),
                Reply::RpcError(code, message) => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
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
    let script = move |tool: &str, args: &Value| {
        if tool == TACHI_AGENT_EVAL_TOOL {
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

fn client(endpoint: &str) -> TachiStaffClient {
    TachiStaffClient::from_config(&config(endpoint), "home").expect("enabled config")
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

// ─────────────────────────────────────────────────────────────────────────
// start
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn legacy_staff_echoing_modern_version_keeps_the_initialized_session() {
    type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
    async fn legacy(
        State(requests): State<Requests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        requests
            .lock()
            .expect("request log")
            .push((headers.clone(), body.clone()));
        let id = body.get("id").cloned();
        match body["method"].as_str().unwrap_or("") {
            "server/discover" => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Unexpected message, expect initialize request",
            )
                .into_response(),
            "initialize" => {
                let mut response = Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": body["params"]["protocolVersion"],
                        "capabilities": { "tools": {} }
                    }
                }))
                .into_response();
                response.headers_mut().insert(
                    "Mcp-Session-Id",
                    "legacy-echo-session".parse().expect("session header"),
                );
                response
            }
            "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
            "tools/call"
                if headers.get("Mcp-Session-Id").and_then(|h| h.to_str().ok())
                    == Some("legacy-echo-session")
                    && !headers.contains_key("Mcp-Method")
                    && body["params"].get("_meta").is_none() =>
            {
                Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "isError": false,
                        "content": [{ "type": "text", "text": working_receipt("d-legacy-echo").to_string() }] }
                }))
                .into_response()
            }
            _ => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        }
    }
    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind legacy echo peer");
    let endpoint = format!(
        "http://{}/mcp",
        listener.local_addr().expect("peer address")
    );
    let app = axum::Router::new()
        .route("/mcp", post(legacy))
        .with_state(requests.clone());
    reiclaw_log::spawn!(async move {
        axum::serve(listener, app)
            .await
            .expect("legacy peer serves");
    });
    let receipt = client(&endpoint)
        .start(
            "codex",
            "bounded task",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .expect("legacy echo start retains its initialized session");
    assert_eq!(receipt.dispatch_id, "d-legacy-echo");
    let requests = requests.lock().expect("request log");
    let methods: Vec<_> = requests
        .iter()
        .map(|(_, body)| body["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "server/discover",
            "initialize",
            "notifications/initialized",
            "tools/call"
        ]
    );
    assert_eq!(requests[1].1["params"]["protocolVersion"], "2026-07-28");
    assert_eq!(requests[3].0["Mcp-Session-Id"], "legacy-echo-session");
    assert_eq!(requests[3].0["x-tachi-profile"], "standard");
    assert_eq!(requests[3].0["x-tachi-agent-identity"], "reiclaw:home");
}

#[tokio::test]
async fn modern_staff_bootstrap_uses_discover_and_negotiated_request_metadata() {
    type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
    async fn modern(
        State(requests): State<Requests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        requests
            .lock()
            .expect("request log")
            .push((headers.clone(), body.clone()));
        let id = body.get("id").cloned();
        let method = body["method"].as_str().unwrap_or("");
        let wire_ok = headers
            .get("MCP-Protocol-Version")
            .and_then(|h| h.to_str().ok())
            == Some("2026-07-28")
            && headers.get("Mcp-Method").and_then(|h| h.to_str().ok()) == Some(method)
            && body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] == "2026-07-28";
        if method == "initialize" || !wire_ok {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": if method == "initialize" { -32022 } else { -32020 },
                    "message": "modern wire required", "data": { "supportedVersions": ["2026-07-28"] } }
            }))).into_response();
        }
        let result = match method {
            "server/discover" => json!({
                "resultType": "complete", "supportedVersions": ["2026-07-28"],
                "capabilities": { "tools": {} }
            }),
            "tools/call" => json!({
                "resultType": "complete", "isError": false,
                "content": [{ "type": "text", "text": working_receipt("d-modern").to_string() }]
            }),
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
    }
    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind modern peer");
    let endpoint = format!(
        "http://{}/mcp",
        listener.local_addr().expect("modern peer address")
    );
    let app = axum::Router::new()
        .route("/mcp", post(modern))
        .with_state(requests.clone());
    reiclaw_log::spawn!(async move {
        axum::serve(listener, app)
            .await
            .expect("modern peer serves");
    });
    let receipt = client(&endpoint)
        .start(
            "codex",
            "bounded task",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .expect("modern start");
    assert_eq!(receipt.dispatch_id, "d-modern");
    let requests = requests.lock().expect("request log");
    let methods: Vec<_> = requests
        .iter()
        .map(|(_, body)| body["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        ["server/discover", "server/discover", "tools/call"]
    );
    let (headers, call) = requests.last().expect("tool call");
    assert_eq!(headers["x-tachi-profile"], "standard");
    assert_eq!(headers["x-tachi-agent-identity"], "reiclaw:home");
    assert_eq!(headers["Mcp-Name"], TACHI_STAFF_TOOL);
    assert!(!headers.contains_key("Mcp-Session-Id"));
    assert_eq!(
        call["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
        "reiclaw"
    );
    assert_eq!(call["params"]["arguments"]["profile"], "codex_55_review");
}

#[tokio::test]
async fn start_maps_harness_to_profile_and_sends_identity_headers() {
    let (fake, endpoint) = serve(|_, _| Reply::Ok(working_receipt("d-1"))).await;
    let client = client(&endpoint);
    let refs = StaffRefs {
        issue_ref: Some("owner/zeroclaw#381".to_string()),
        ..StaffRefs::default()
    };

    let receipt = client
        .start(
            "codex",
            "Have Codex review the claude.rs provider",
            StaffingReason::ExplicitUserRequest,
            &refs,
        )
        .await
        .expect("accepted start");
    assert_eq!(
        receipt,
        StaffReceipt {
            dispatch_id: "d-1".to_string(),
            state: RunState::Working,
            run_dir: "/home/owner/.tachi/runs/d-1".to_string(),
        }
    );

    let calls = fake.calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.tool, TACHI_STAFF_TOOL);
    assert_eq!(call.args["action"], "start");
    assert_eq!(call.args["profile"], "codex_55_review");
    assert_eq!(call.args["staffing_reason"], "explicit_user_request");
    // Harness names in the task are ordinary content (ADR-017 §3).
    assert_eq!(
        call.args["task"],
        "Have Codex review the claude.rs provider"
    );
    assert_eq!(call.args["project"], "zeroclaw");
    assert_eq!(call.args["issue_ref"], "owner/zeroclaw#381");
    assert!(call.args.get("pr_ref").is_none());
    // The body names a profile, never a worker, command, or placement.
    assert!(call.args.get("worker").is_none());
    let keys: BTreeSet<&str> = call
        .args
        .as_object()
        .expect("object args")
        .keys()
        .map(String::as_str)
        .collect();
    for key in &keys {
        assert!(
            TACHI_STAFF_PARAM_FIELDS.contains(key),
            "`{key}` is not a TachiStaffParams field; Tachi would refuse the call"
        );
    }

    assert_eq!(call.headers[HEADER_PROFILE], "standard");
    assert_eq!(call.headers[HEADER_AGENT_IDENTITY], "reiclaw:home");
    assert_eq!(call.headers[HEADER_CLIENT], "reiclaw");
    assert_eq!(call.headers[HEADER_PROJECT], "zeroclaw");
    assert_eq!(call.headers["mcp-session-id"], "session-1");
}

#[tokio::test]
async fn unknown_harness_and_empty_task_never_reach_tachi() {
    let (fake, endpoint) = serve(|_, _| Reply::Ok(working_receipt("never"))).await;
    let client = client(&endpoint);

    let err = client
        .start(
            "grok",
            "do it",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, TachiStaffError::UnknownHarness("grok".to_string()));

    let err = client
        .start(
            "codex",
            "   ",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TachiStaffError::Refused(_)), "{err:?}");

    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_session_is_reused_across_calls() {
    let (fake, endpoint) = serve(|_, args| {
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING",
            "status_revision": 1
        }))
    })
    .await;
    let client = client(&endpoint);
    client.status("d-1").await.expect("first");
    client.status("d-1").await.expect("second");
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 1);
    assert_eq!(fake.calls().len(), 2);
}

// ─────────────────────────────────────────────────────────────────────────
// status / result
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_reads_the_canonical_receipt_and_ignores_extra_fields() {
    let (fake, endpoint) = serve(|_, args| {
        let state = match args["dispatch_id"].as_str() {
            Some("done") => "TASK_STATE_COMPLETED",
            Some("partial") => "TASK_STATE_INPUT_REQUIRED",
            _ => "TASK_STATE_WORKING",
        };
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": state,
            "status_revision": 7,
            "closure_kind": "partial",
            "result_written": state == "TASK_STATE_COMPLETED",
            "updated_at": "2026-09-26T10:00:00Z",
            "agent": "codex",
            "authority": { "enforced_by": "certified" },
            "identity_receipt": { "planned": { "model": "x" } },
            "read_projection": { "execution_state": "running" },
            "status": "completed",
            "action": "status"
        }))
    })
    .await;
    let client = client(&endpoint);

    let running = client.status("d-1").await.expect("status");
    assert_eq!(running.state, RunState::Working);
    assert_eq!(running.revision, Some(7));
    assert_eq!(
        running.read_projection.as_ref().unwrap()["execution_state"],
        "running"
    );
    assert!(!running.is_terminal());

    let done = client.status("done").await.expect("status");
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(done.result_written, Some(true));
    assert!(done.is_terminal());

    let partial = client.status("partial").await.expect("status");
    assert!(partial.is_terminal(), "INPUT_REQUIRED + partial is closed");

    let (tool, args) = &fake.calls()[0];
    assert_eq!(tool, TACHI_STAFF_TOOL);
    assert_eq!(
        args,
        &json!({ "action": "status", "format": "json", "dispatch_id": "d-1" })
    );
}

// Staff result wire shape: Tachi #2039, pinned at b3d82a0287b52b793876ab5bc883741feea66adf,
// tools/workflow_facade.rs:90 and tools/task_facade.rs:104,205. The separate
// external-staffing golden above freezes start/lifecycle, not this read shape.
#[tokio::test]
async fn result_reads_full_unicode_report_through_staff_and_preserves_absence() {
    let unicode = "报告🙂\n".repeat(2_500);
    assert!(unicode.chars().count() > 8_000);
    let expected = unicode.clone();
    let (fake, endpoint) = serve(move |tool, args| {
        assert_eq!(tool, TACHI_STAFF_TOOL);
        assert_eq!(args["action"], "result");
        let body = match args["dispatch_id"].as_str() {
            Some("done") => json!({
                "body":unicode, "truncated":false,
                "full_size_chars":unicode.chars().count(), "full_size_bytes":unicode.len()
            }),
            Some("empty") => json!({"body":"", "truncated":false}),
            Some("truncated") => json!({"body":"报告🙂", "truncated":true}),
            _ => json!({"body":null, "note":"no result.md found in run directory"}),
        };
        Reply::Ok(json!({
            "dispatch_id":args["dispatch_id"],
            "state":if args["dispatch_id"] == "pending" {"TASK_STATE_WORKING"} else {"TASK_STATE_COMPLETED"},
            "status_revision":4,
            "result":body
        }))
    })
    .await;
    let client = client(&endpoint);

    let done = client.result("done").await.expect("result");
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(done.status.revision, Some(4));
    assert_eq!(done.body.as_deref(), Some(expected.as_str()));
    assert!(!done.truncated);

    let pending = client.result("pending").await.expect("result");
    assert_eq!(pending.state, RunState::Working);
    assert!(!pending.status.is_terminal());
    assert_eq!(pending.body, None);
    assert!(!pending.truncated);
    let empty = client.result("empty").await.expect("empty report");
    assert_eq!(empty.body.as_deref(), Some(""));
    let truncated = client
        .result("truncated")
        .await
        .expect("reported truncation");
    assert_eq!(truncated.body.as_deref(), Some("报告🙂"));
    assert!(truncated.truncated);

    let calls = fake.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls[0],
        (
            TACHI_STAFF_TOOL.to_string(),
            json!({"action":"result", "format":"json", "dispatch_id":"done"})
        )
    );
    assert!(calls.iter().all(|(tool, args)| tool == TACHI_STAFF_TOOL
        && args["action"] == "result"
        && args.get("include_result").is_none()));
}

#[tokio::test]
async fn result_enforces_the_full_report_byte_bound_without_cutting_unicode() {
    let full = "🙂".repeat(16 * 1024);
    assert_eq!(full.len(), 64 * 1024);
    let expected = full.clone();
    let (fake, endpoint) = serve(move |_, args| {
        let body = if args["dispatch_id"] == "at-limit" {
            full.clone()
        } else {
            format!("{full}a")
        };
        Reply::Ok(json!({
            "dispatch_id":args["dispatch_id"], "state":"TASK_STATE_COMPLETED",
            "result":{"body":body, "truncated":false}
        }))
    })
    .await;
    let client = client(&endpoint);
    let at_limit = client.result("at-limit").await.unwrap();
    assert_eq!(at_limit.body.as_deref(), Some(expected.as_str()));
    assert!(!at_limit.truncated);
    let over_limit = client.result("over-limit").await.unwrap_err();
    assert!(
        matches!(over_limit, TachiStaffError::Protocol(_)),
        "{over_limit:?}"
    );
    assert_eq!(fake.calls().len(), 2, "no report fallback or retry");
}

#[tokio::test]
async fn result_requires_the_canonical_receipt_and_explicit_report_body() {
    let (fake, endpoint) = serve(|_, args| {
        let mut payload = json!({
            "dispatch_id":args["dispatch_id"], "state":"TASK_STATE_COMPLETED",
            "result":{"body":"report", "truncated":false}
        });
        let object = payload.as_object_mut().unwrap();
        match args["dispatch_id"].as_str().unwrap() {
            "missing-dispatch" => {
                object.remove("dispatch_id");
            }
            "missing-state" => {
                object.remove("state");
            }
            "missing-result" => {
                object.remove("result");
            }
            "null-result" => {
                object.insert("result".into(), Value::Null);
            }
            "missing-body" => {
                object.insert("result".into(), json!({"truncated":false}));
            }
            "non-text-body" => {
                object.insert("result".into(), json!({"body":7}));
            }
            "legacy-task" => {
                object.remove("dispatch_id");
                object.insert(
                    "run_status".into(),
                    json!({"dispatch_id":"legacy-task", "state":"TASK_STATE_COMPLETED"}),
                );
            }
            _ => panic!("unexpected result case"),
        }
        Reply::Ok(payload)
    })
    .await;
    let client = client(&endpoint);
    let cases = [
        "missing-dispatch",
        "missing-state",
        "missing-result",
        "null-result",
        "missing-body",
        "non-text-body",
        "legacy-task",
    ];
    for case in cases {
        let error = client.result(case).await.unwrap_err();
        assert!(
            matches!(error, TachiStaffError::Protocol(_)),
            "{case}: {error:?}"
        );
    }
    let calls = fake.calls();
    assert_eq!(calls.len(), cases.len());
    assert!(
        calls
            .iter()
            .all(|(tool, args)| tool == TACHI_STAFF_TOOL && args["action"] == "result")
    );
}

#[tokio::test]
async fn result_reopens_a_stale_read_session_at_most_once() {
    let (fake, endpoint) = serve(|_, _| Reply::Http(StatusCode::NOT_FOUND)).await;
    let error = client(&endpoint).result("d-stale").await.unwrap_err();
    assert!(
        matches!(error, TachiStaffError::Unavailable(_)),
        "{error:?}"
    );
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 2);
    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|(tool, args)| tool == TACHI_STAFF_TOOL && args["action"] == "result")
    );
}

// ─────────────────────────────────────────────────────────────────────────
// cancel
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_maps_every_tachi_receipt_to_a_typed_outcome() {
    let (fake, endpoint) = serve(|_, args| {
        let unavailable = |reason: &str, observed: Value| {
            Reply::Ok(json!({
                "receipt": "cancellation_unavailable",
                "dispatch_id": args["dispatch_id"],
                "expected_status_revision": args["expected_status_revision"],
                "observed_status_revision": observed,
                "reason": reason,
                "lifecycle_owner": "unknown_or_unavailable",
                "backend": "unknown_or_unavailable",
                "state": null
            }))
        };
        match args["expected_status_revision"].as_u64() {
            Some(1) => unavailable("non_managed_custom_execution", json!(1)),
            Some(2) => unavailable("stale_status_revision", json!(5)),
            Some(3) => Reply::Ok(json!({
                "receipt": "cancellation_confirmed",
                "dispatch_id": args["dispatch_id"],
                "state": "TASK_STATE_CANCELED",
                "lifecycle_owner": "memory_server_managed_custom",
                "backend": "custom"
            })),
            Some(4) => Reply::Ok(json!({
                "receipt": "cancellation_requested",
                "state": "TASK_STATE_WORKING"
            })),
            _ => unavailable("terminal_or_recovery_state", json!(9)),
        }
    })
    .await;
    let client = client(&endpoint);

    // CLI-backed runs: Tachi cannot cancel them, and the body says so.
    assert_eq!(
        client.cancel("cli-run", 1).await.unwrap_err(),
        TachiStaffError::CancelUnsupported {
            reason: "non_managed_custom_execution".to_string()
        }
    );
    assert_eq!(
        client.cancel("d-1", 2).await.unwrap_err(),
        TachiStaffError::StaleRevision {
            expected: 2,
            observed: Some(5)
        }
    );
    let confirmed = client.cancel("custom-run", 3).await.expect("confirmed");
    assert_eq!(confirmed.outcome, CancelOutcome::Confirmed);
    assert_eq!(confirmed.state, Some(RunState::Canceled));
    let requested = client.cancel("custom-run", 4).await.expect("requested");
    assert_eq!(requested.outcome, CancelOutcome::Requested);
    assert!(matches!(
        client.cancel("done", 9).await.unwrap_err(),
        TachiStaffError::Refused(reason) if reason.contains("terminal_or_recovery_state")
    ));

    let (_, args) = &fake.calls()[0];
    assert_eq!(
        args,
        &json!({
            "action": "cancel",
            "dispatch_id": "cli-run",
            "expected_status_revision": 1
        })
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Typed failures and fail-closed
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn content_block_first_payload_remains_authoritative() {
    let (fake, endpoint) = serve(|_, args| {
        let primary = json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING",
            "status_revision": 4
        });
        let later = json!({
            "dispatch_id": "wrong-dispatch",
            "state": "TASK_STATE_COMPLETED",
            "status_revision": 99
        });
        Reply::ToolResult(json!({"content": [
            {"type":"text", "text":primary.to_string()},
            {"type":"text", "text":later.to_string()}
        ], "isError":false}))
    })
    .await;
    let client = client(&endpoint);
    let status = client.status("d-primary").await.expect("primary status");
    assert_eq!(status.dispatch_id, "d-primary");
    assert_eq!(status.state, RunState::Working);
    assert_eq!(status.revision, Some(4));
    assert!(!status.is_terminal());
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test]
async fn content_block_invalid_primary_cannot_be_salvaged() {
    let (fake, endpoint) = serve(|_, args| {
        let case = args["dispatch_id"].as_str().expect("scripted case");
        let payload = json!({"dispatch_id":case, "state":"TASK_STATE_COMPLETED"});
        let later = json!({"type":"text", "text":payload.to_string()});
        let primary = match case {
            "missing-content" => return Reply::ToolResult(json!({"isError":false})),
            "non-array-content" => {
                return Reply::ToolResult(json!({"content":later, "isError":false}));
            }
            "empty-content" => {
                return Reply::ToolResult(json!({"content":[], "isError":false}));
            }
            "non-text-primary" => json!({"type":"image", "data":"synthetic"}),
            "non-text-with-json" => json!({"type":"image", "text":payload.to_string()}),
            "untyped-primary" => json!({}),
            "untyped-with-json" => json!({"text":payload.to_string()}),
            "missing-text" => json!({"type":"text"}),
            "non-string-text" => json!({"type":"text", "text":7}),
            "empty-text" => json!({"type":"text", "text":""}),
            "malformed-primary" => json!({"type":"text", "text":"{"}),
            "trailing-primary" => {
                json!({"type":"text", "text":format!("{payload}\ntrailing garbage")})
            }
            "two-values-primary" => {
                json!({"type":"text", "text":format!("{payload}\n{payload}")})
            }
            _ => panic!("unexpected case {case}"),
        };
        Reply::ToolResult(json!({"content":[primary, later], "isError":false}))
    })
    .await;
    let client = client(&endpoint);
    let cases = [
        "missing-content",
        "non-array-content",
        "empty-content",
        "non-text-primary",
        "non-text-with-json",
        "untyped-primary",
        "untyped-with-json",
        "missing-text",
        "non-string-text",
        "empty-text",
        "malformed-primary",
        "trailing-primary",
        "two-values-primary",
    ];
    for case in cases {
        let error = client.status(case).await.unwrap_err();
        assert!(
            matches!(error, TachiStaffError::Protocol(_)),
            "{case}: {error:?}"
        );
    }
    assert_eq!(fake.calls().len(), cases.len(), "no alternate-block retry");
}

#[tokio::test]
async fn content_block_refusal_retains_all_text_diagnostics() {
    let (fake, endpoint) = serve(|_, args| {
        if args["dispatch_id"] == "empty-error" {
            return Reply::ToolResult(json!({"content":[], "isError":true}));
        }
        Reply::ToolResult(json!({"content":[
            {"type":"text", "text":working_receipt("not-accepted").to_string()},
            {"type":"text", "text":"policy denied"},
            {"type":"text", "text":"independent warning"}
        ], "isError":true}))
    })
    .await;
    let client = client(&endpoint);
    let expected = TachiStaffError::Refused(format!(
        "{}\npolicy denied\nindependent warning",
        working_receipt("not-accepted")
    ));
    assert_eq!(client.status("denied").await.unwrap_err(), expected);
    assert_eq!(
        client
            .start(
                "codex",
                "review the adapter",
                StaffingReason::ExplicitUserRequest,
                &StaffRefs::default(),
            )
            .await
            .unwrap_err(),
        expected
    );
    assert_eq!(
        client.status("empty-error").await.unwrap_err(),
        TachiStaffError::Refused("tachi_staff returned an error without detail".to_string())
    );
    assert_eq!(fake.calls().len(), 3);
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tachi_failures_are_typed() {
    let (fake, endpoint) = serve(|_, args| match args["dispatch_id"].as_str() {
        Some("tool-error") => {
            Reply::ToolError("staff_status: unknown dispatch_id \"tool-error\"".to_string())
        }
        Some("rpc-error") => Reply::RpcError(-32602, "unknown field `cwd`".to_string()),
        Some("not-json") => Reply::Text("## Tachi staff status".to_string()),
        Some("missing-state") => Reply::Ok(json!({ "dispatch_id": "missing-state" })),
        _ => Reply::Http(StatusCode::INTERNAL_SERVER_ERROR),
    })
    .await;
    let client = client(&endpoint);

    assert!(matches!(
        client.status("tool-error").await.unwrap_err(),
        TachiStaffError::Refused(detail) if detail.contains("unknown dispatch_id")
    ));
    assert!(matches!(
        client.status("rpc-error").await.unwrap_err(),
        TachiStaffError::Refused(detail) if detail.contains("-32602")
    ));
    assert!(matches!(
        client.status("not-json").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
    assert!(matches!(
        client.status("missing-state").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
    assert!(matches!(
        client.status("http-500").await.unwrap_err(),
        TachiStaffError::Unavailable(_)
    ));
    // Refusals keep the session; the transport failure dropped it, so the
    // next call opens a fresh one.
    assert!(client.status("tool-error").await.is_err());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn tachi_down_is_unavailable_and_nothing_runs() {
    // A port that nothing listens on.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    drop(listener);
    let client = client(&endpoint);

    let err = client
        .start(
            "codex",
            "fix the flaky test",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TachiStaffError::Unavailable(_)), "{err:?}");
    assert!(matches!(
        client.status("d-1").await.unwrap_err(),
        TachiStaffError::Unavailable(_)
    ));
    // Nothing local ran: the client module holds no process capability
    // (`tests::module_source_scans_hold` scans staff.rs).
}

#[test]
fn disabled_or_invalid_tachi_config_fails_closed() {
    let disabled = TachiConfig::default();
    assert!(matches!(
        TachiStaffClient::from_config(&disabled, "home").unwrap_err(),
        TachiStaffError::Unavailable(reason) if reason.contains("not enabled")
    ));
    let remote = TachiConfig {
        enabled: true,
        endpoint: "http://192.168.1.9:6919/mcp".to_string(),
        ..TachiConfig::default()
    };
    assert!(matches!(
        TachiStaffClient::from_config(&remote, "home").unwrap_err(),
        TachiStaffError::Unavailable(reason) if reason.contains("loopback")
    ));
}

#[tokio::test]
async fn stale_session_after_tachi_restart_is_reopened_once() {
    let (fake, endpoint) = serve(|_, args| {
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING"
        }))
    })
    .await;
    let client = client(&endpoint);
    client.status("d-1").await.expect("warm session");

    fake.expire_next_call.store(true, Ordering::SeqCst);
    let status = client.status("d-1").await.expect("reopened");
    assert_eq!(status.revision, None);
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 2);
    // The expired request was refused by HTTP 404 before Tachi handled it,
    // so only the two answered calls were recorded.
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn wait_until_terminal_polls_at_the_configured_interval() {
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = polls.clone();
    let (_fake, endpoint) = serve(move |_, _| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let state = if n >= 2 {
            "TASK_STATE_COMPLETED"
        } else {
            "TASK_STATE_WORKING"
        };
        Reply::Ok(json!({ "dispatch_id": "d-1", "state": state }))
    })
    .await;
    assert_eq!(client(&endpoint).poll_interval().as_secs(), 15, "default");
    let client = TachiStaffClient::from_config(
        &TachiConfig {
            poll_secs: 1,
            ..config(&endpoint)
        },
        "home",
    )
    .expect("config");
    let started = std::time::Instant::now();
    let status = client
        .wait_until_terminal("d-1", std::time::Duration::from_secs(3600))
        .await
        .expect("terminal");
    assert_eq!(status.state, RunState::Completed);
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "two waits of poll_secs between three polls"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Contract pins
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn golden_staffing_contract_matches_our_receipt_and_terminal_rules() {
    let contract: Value = serde_json::from_str(STAFFING_CONTRACT_FIXTURE).expect("fixture is JSON");
    assert_eq!(contract["schema_version"], "external_staffing.v1");

    // An accepted start carrying exactly the contract's required fields
    // (plus anything else) deserializes into our receipt.
    let required: Vec<&str> = contract["accepted_start"]["required_fields"]
        .as_array()
        .expect("required_fields")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let initial_state = contract["accepted_start"]["initial_state"]
        .as_str()
        .expect("initial_state");
    let mut wire = serde_json::Map::new();
    for field in &required {
        let value = match *field {
            "state" => initial_state.to_string(),
            other => format!("{other}-value"),
        };
        wire.insert((*field).to_string(), Value::String(value));
    }
    wire.insert("unlisted".to_string(), json!(1));
    let receipt: StaffReceipt =
        serde_json::from_value(Value::Object(wire.clone())).expect("receipt from contract");
    assert_eq!(receipt.state, RunState::Working);
    // Dropping any required field must fail: we rely on all of them.
    for field in &required {
        let mut partial = wire.clone();
        partial.remove(*field);
        assert!(
            serde_json::from_value::<StaffReceipt>(Value::Object(partial)).is_err(),
            "receipt must require `{field}`"
        );
    }

    let status = |state: &str, closure: Option<&str>| RunStatus {
        dispatch_id: "d".to_string(),
        state: RunState::from_wire(state),
        revision: None,
        closure_kind: closure.map(str::to_string),
        result_written: None,
        updated_at: None,
        read_projection: None,
        native: Default::default(),
    };
    for state in contract["terminal_semantics"]["terminal_states"]
        .as_array()
        .expect("terminal_states")
    {
        let state = state.as_str().expect("state string");
        assert!(status(state, None).is_terminal(), "{state} is terminal");
        assert_eq!(RunState::from_wire(state).as_wire(), state);
    }
    let partial = &contract["terminal_semantics"]["closed_partial"];
    let partial_state = partial["state"].as_str().expect("partial state");
    assert!(status(partial_state, partial["closure_kind"].as_str()).is_terminal());
    assert!(!status(partial_state, None).is_terminal());
    assert!(!status(initial_state, None).is_terminal());
}

#[test]
fn staffing_reasons_use_tachi_wire_spelling() {
    let spelled: Vec<&str> = StaffingReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        spelled,
        [
            "explicit_user_request",
            "durable_cross_session",
            "cross_device_remote",
            "native_subagent_unavailable",
        ]
    );
    for reason in StaffingReason::ALL {
        assert_eq!(
            serde_json::to_value(reason).expect("serialize"),
            json!(reason.as_str())
        );
        assert_eq!(StaffingReason::parse(reason.as_str()), Some(reason));
    }
    assert_eq!(StaffingReason::parse("because"), None);
}

/// Live check against a real local Tachi daemon: `TACHI_LIVE=1 cargo test -p
/// zeroclaw-runtime tachi_live -- --ignored`. It only reads the status of a
/// dispatch id that cannot exist, so it starts nothing; it proves the
/// endpoint, the handshake, the headers, and the typed refusal path.
#[tokio::test]
#[ignore = "needs a running Tachi daemon on 127.0.0.1:6919 and TACHI_LIVE=1"]
async fn tachi_live_status_of_unknown_run_is_a_typed_refusal() {
    if std::env::var("TACHI_LIVE").as_deref() != Ok("1") {
        return;
    }
    let config = TachiConfig {
        enabled: true,
        ..TachiConfig::default()
    };
    let client = TachiStaffClient::from_config(&config, "live-test").expect("config");
    let err = client
        .status("zeroclaw-live-probe-does-not-exist")
        .await
        .unwrap_err();
    assert!(matches!(err, TachiStaffError::Refused(_)), "{err:?}");
}

#[tokio::test]
async fn transmitted_start_on_stale_session_is_unknown_and_never_replayed() {
    let (fake, endpoint) = serve(|_, _| Reply::Http(StatusCode::NOT_FOUND)).await;
    let error = client(&endpoint)
        .start(
            "codex",
            "review Codex on GitHub together",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, TachiStaffError::SubmissionUnknown(_)),
        "{error:?}"
    );
    assert_eq!(
        fake.calls().len(),
        1,
        "404 after POST must not trigger another start"
    );
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn result_keeps_one_canonical_receipt_and_never_fabricates_task_projections() {
    let projection = json!({
        "execution_state":"orphaned", "control_state":"unavailable", "outcome_state":"unknown"
    });
    let expected = projection.clone();
    let (fake, endpoint) = serve(move |_, _| {
        Reply::Ok(json!({
            "dispatch_id":"d-1", "state":"TASK_STATE_WORKING", "status_revision":9,
            "read_projection":projection,
            "result":{"body":"Tests pass according to worker", "truncated":false},
            "receipt_result":{"exit_code":0},
            "run_status":{"dispatch_id":"other", "state":"TASK_STATE_COMPLETED"},
            "task":{"state_basis":"run_stale_timeout"},
            "managed_run":{"classification":"managed_custom", "cancel_available":true}
        }))
    })
    .await;
    let report = client(&endpoint).result("d-1").await.unwrap();
    assert_eq!(report.state, RunState::Working);
    assert_eq!(report.status.dispatch_id, "d-1");
    assert_eq!(report.status.state, RunState::Working);
    assert_eq!(report.status.revision, Some(9));
    assert_eq!(report.status.read_projection, Some(expected));
    assert!(!report.status.is_terminal());
    assert_eq!(
        report.body.as_deref(),
        Some("Tests pass according to worker")
    );
    assert_eq!(report.task_state, None);
    assert_eq!(report.state_basis, None);
    assert_eq!(report.managed_run, None);
    let wire = serde_json::to_value(report).unwrap();
    for key in ["task_state", "state_basis", "managed_run"] {
        assert_eq!(
            wire.get(key),
            Some(&Value::Null),
            "retain the legacy key {key}"
        );
    }
    assert_eq!(fake.calls().len(), 1, "all facts come from one response");
}

#[tokio::test]
async fn mismatched_dispatch_receipts_are_not_admitted() {
    let (fake, endpoint) = serve(|_, _| {
        Reply::Ok(json!({
            "dispatch_id":"wrong", "state":"TASK_STATE_COMPLETED",
            "result":{"body":"success", "truncated":false}
        }))
    })
    .await;
    let client = client(&endpoint);
    assert!(matches!(
        client.status("d-1").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
    assert!(matches!(
        client.result("d-1").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
    assert_eq!(
        fake.calls().len(),
        2,
        "no alternate source for mismatched receipts"
    );
}

#[tokio::test]
async fn result_preserves_unknown_canonical_states() {
    let (_, endpoint) = serve(|_, _| {
        Reply::Ok(json!({
            "dispatch_id":"d-new", "state":"TASK_STATE_FUTURE",
            "result":{"body":null}
        }))
    })
    .await;
    let report = client(&endpoint).result("d-new").await.unwrap();
    assert_eq!(report.state, RunState::Other("TASK_STATE_FUTURE".into()));
    assert_eq!(report.status.state, report.state);
    assert!(!report.status.is_terminal());
}

#[tokio::test]
async fn bounded_watch_returns_last_observation_without_claiming_completion() {
    let (fake, endpoint) =
        serve(|_, _| Reply::Ok(json!({"dispatch_id":"d-1", "state":"TASK_STATE_WORKING"}))).await;
    let client = client(&endpoint);
    client.status("d-1").await.unwrap(); // isolate polling from session setup
    let calls_before = fake.calls().len();
    let status = client
        .wait_until_terminal("d-1", std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert!(!status.is_terminal());
    assert_eq!(fake.calls().len() - calls_before, 1);
}

// ADR-025 §8: continue_run, finish_run, proxy-continuation gates, and briefs
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn continue_run_sends_tachi_staff_params_and_maps_receipt() {
    let (fake, endpoint) = serve(|tool, args| {
        assert_eq!(tool, TACHI_STAFF_TOOL);
        assert_eq!(args["action"], "continue");
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING",
            "status_revision": 8,
            "extra_future_field": true
        }))
    })
    .await;
    let client = client(&endpoint);

    let receipt = client
        .continue_run("d-cont", 7, "  Proceed with option A  ")
        .await
        .expect("continue accepted");
    assert_eq!(
        receipt,
        ContinueReceipt {
            dispatch_id: "d-cont".to_string(),
            state: RunState::Working,
            status_revision: Some(8),
        }
    );

    let calls = fake.staff_calls();
    assert_eq!(calls.len(), 1);
    let args = &calls[0].1;
    assert_eq!(
        args,
        &json!({
            "action": "continue",
            "format": "json",
            "dispatch_id": "d-cont",
            "expected_status_revision": 7,
            "task": "Proceed with option A"
        })
    );
    for key in args.as_object().expect("object args").keys() {
        assert!(
            TACHI_STAFF_PARAM_FIELDS.contains(&key.as_str()),
            "`{key}` is not a TachiStaffParams field"
        );
    }
}

#[tokio::test]
async fn continue_and_finish_reject_empty_inputs_before_contacting_tachi() {
    let (fake, endpoint) = serve(|_, _| panic!("empty inputs must never reach Tachi")).await;
    let client = client(&endpoint);

    for bad_id in ["", "   \n\t "] {
        assert!(matches!(
            client.continue_run(bad_id, 1, "answer").await.unwrap_err(),
            TachiStaffError::Refused(_)
        ));
        assert!(matches!(
            client.finish_run(bad_id, 1).await.unwrap_err(),
            TachiStaffError::Refused(_)
        ));
    }
    for bad_task in ["", "   \n\t "] {
        assert!(matches!(
            client.continue_run("d-1", 1, bad_task).await.unwrap_err(),
            TachiStaffError::Refused(_)
        ));
    }

    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn continue_and_finish_map_stale_revision_and_unsupported_refusals() {
    let (fake, endpoint) = serve(|_, args| match args["action"].as_str() {
        Some("continue") if args["expected_status_revision"] == 2 => Reply::Ok(json!({
            "receipt": "continue_unavailable",
            "reason": "stale_status_revision",
            "observed_status_revision": 5,
            "state": null
        })),
        Some("continue") if args["expected_status_revision"] == 3 => Reply::ToolError(
            r#"{"reason":"stale_status_revision","observed_status_revision":6}"#.into(),
        ),
        Some("continue") => Reply::ToolError("tachi_staff: unsupported action 'continue'".into()),
        Some("finish") if args["expected_status_revision"] == 4 => Reply::Ok(json!({
            "receipt": "finish_unavailable",
            "reason": "stale_status_revision",
            "observed_status_revision": 9
        })),
        Some("finish") => Reply::ToolError("tachi_staff: unsupported action 'finish'".into()),
        _ => panic!("unexpected call {args}"),
    })
    .await;
    let client = client(&endpoint);

    assert_eq!(
        client.continue_run("d-1", 2, "ok").await.unwrap_err(),
        TachiStaffError::StaleRevision {
            expected: 2,
            observed: Some(5)
        }
    );
    assert_eq!(
        client.continue_run("d-1", 3, "ok").await.unwrap_err(),
        TachiStaffError::StaleRevision {
            expected: 3,
            observed: Some(6)
        }
    );
    assert!(matches!(
        client.continue_run("d-1", 1, "ok").await.unwrap_err(),
        TachiStaffError::Refused(msg) if msg.contains("unsupported action 'continue'")
    ));
    assert_eq!(
        client.finish_run("d-1", 4).await.unwrap_err(),
        TachiStaffError::StaleRevision {
            expected: 4,
            observed: Some(9)
        }
    );
    assert!(matches!(
        client.finish_run("d-1", 1).await.unwrap_err(),
        TachiStaffError::Refused(msg) if msg.contains("unsupported action 'finish'")
    ));
    assert_eq!(fake.staff_calls().len(), 5);
}

#[tokio::test]
async fn continue_and_finish_return_submission_unknown_on_post_send_drop_without_retry() {
    let (fake, endpoint) = serve(|_, args| match args["dispatch_id"].as_str() {
        Some("drop-500") => Reply::Http(StatusCode::INTERNAL_SERVER_ERROR),
        Some("drop-404") => Reply::Http(StatusCode::NOT_FOUND),
        Some("bad-json") => Reply::Text("truncated".to_string()),
        _ => Reply::Ok(json!({"dispatch_id": "missing-state"})),
    })
    .await;
    let client = client(&endpoint);

    for case in ["drop-500", "drop-404", "bad-json", "missing-state"] {
        let calls_before = fake.calls().len();
        let cont_err = client.continue_run(case, 1, "answer").await.unwrap_err();
        assert!(
            matches!(cont_err, TachiStaffError::SubmissionUnknown(_)),
            "continue {case}: {cont_err:?}"
        );
        assert_eq!(
            fake.calls().len() - calls_before,
            1,
            "continue {case} must never retry after transmission"
        );

        let calls_before = fake.calls().len();
        let fin_err = client.finish_run(case, 1).await.unwrap_err();
        assert!(
            matches!(fin_err, TachiStaffError::SubmissionUnknown(_)),
            "finish {case}: {fin_err:?}"
        );
        assert_eq!(
            fake.calls().len() - calls_before,
            1,
            "finish {case} must never retry after transmission"
        );
    }
}

#[tokio::test]
async fn finish_run_sends_tachi_staff_params_and_maps_receipt() {
    let (fake, endpoint) = serve(|tool, args| {
        assert_eq!(tool, TACHI_STAFF_TOOL);
        assert_eq!(args["action"], "finish");
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING",
            "status_revision": 12,
            "controller_epoch_id": "fixture-epoch",
            "session": {"phase": "finished", "session_id": "fixture-session", "turn": 2}
        }))
    })
    .await;
    let client = client(&endpoint);

    let receipt = client
        .finish_run("d-fin", 11)
        .await
        .expect("finish accepted");
    assert_eq!(
        receipt,
        FinishReceipt {
            dispatch_id: "d-fin".to_string(),
            state: RunState::Working,
            status_revision: Some(12),
        }
    );

    let calls = fake.staff_calls();
    assert_eq!(calls.len(), 1);
    let args = &calls[0].1;
    assert_eq!(
        args,
        &json!({
            "action": "finish",
            "format": "json",
            "dispatch_id": "d-fin",
            "expected_status_revision": 11
        })
    );
    for key in args.as_object().expect("object args").keys() {
        assert!(
            TACHI_STAFF_PARAM_FIELDS.contains(&key.as_str()),
            "`{key}` is not a TachiStaffParams field"
        );
    }
}

#[tokio::test]
async fn native_success_shaped_reply_without_state_is_unknown_and_not_retried() {
    let (fake, endpoint) = serve(|tool, args| {
        assert_eq!(tool, TACHI_STAFF_TOOL);
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "status_revision": 12,
            "controller_epoch_id": "fixture-epoch",
            "session": {"phase": "instruction_accepted", "session_id": "fixture-session", "turn": 1}
        }))
    })
    .await;
    let client = client(&endpoint);
    assert!(matches!(
        client.continue_run("d-native", 11, "Check once.").await,
        Err(TachiStaffError::SubmissionUnknown(_))
    ));
    assert!(matches!(
        client.finish_run("another-native", 11).await,
        Err(TachiStaffError::SubmissionUnknown(_))
    ));
    let calls = fake.staff_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1["action"], "continue");
    assert_eq!(calls[1].1["action"], "finish");
}

#[tokio::test]
async fn proxy_continuation_gate_enforces_cap_and_refuses_auth_required_and_partial() {
    let round = Arc::new(AtomicUsize::new(0));
    let round_counter = round.clone();
    let (fake, endpoint) = serve(move |_, args| {
        assert_eq!(args["action"], "continue");
        let n = round_counter.fetch_add(1, Ordering::SeqCst) + 1;
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING",
            "status_revision": n as u64 + 10
        }))
    })
    .await;
    let client = client(&endpoint);
    let mut gate = ProxyContinuationGate::new("d-gate");

    let auth_status = RunStatus {
        dispatch_id: "d-gate".to_string(),
        state: RunState::Other("TASK_STATE_AUTH_REQUIRED".to_string()),
        revision: Some(10),
        closure_kind: None,
        result_written: None,
        updated_at: None,
        read_projection: None,
        native: Default::default(),
    };
    assert_eq!(
        gate.continue_run(&client, &auth_status, "approved")
            .await
            .unwrap_err(),
        ProxyContinuationError::AuthRequired
    );

    let partial_status = RunStatus {
        dispatch_id: "d-gate".to_string(),
        state: RunState::InputRequired,
        revision: Some(10),
        closure_kind: Some("partial".to_string()),
        result_written: None,
        updated_at: None,
        read_projection: None,
        native: Default::default(),
    };
    assert_eq!(
        gate.continue_run(&client, &partial_status, "proceed")
            .await
            .unwrap_err(),
        ProxyContinuationError::PartialClosure
    );
    assert!(
        fake.calls().is_empty(),
        "refused gates must not contact Tachi"
    );

    for rev in 0..u64::from(MAX_PROXY_CONTINUATIONS_PER_DISPATCH) {
        let ready = RunStatus {
            dispatch_id: "d-gate".to_string(),
            state: RunState::InputRequired,
            revision: Some(10 + rev),
            closure_kind: None,
            result_written: None,
            updated_at: None,
            read_projection: None,
            native: Default::default(),
        };
        let receipt = gate
            .continue_run(&client, &ready, &format!("Grounded answer {rev}"))
            .await
            .expect("admitted continuation");
        assert_eq!(receipt.state, RunState::Working);
    }
    assert_eq!(
        gate.prior_auto_continuations(),
        MAX_PROXY_CONTINUATIONS_PER_DISPATCH
    );
    assert_eq!(gate.remaining_auto_continuations(), 0);

    let fourth = RunStatus {
        dispatch_id: "d-gate".to_string(),
        state: RunState::InputRequired,
        revision: Some(20),
        closure_kind: None,
        result_written: None,
        updated_at: None,
        read_projection: None,
        native: Default::default(),
    };
    assert_eq!(
        gate.continue_run(&client, &fourth, "Fourth round")
            .await
            .unwrap_err(),
        ProxyContinuationError::CapExceeded {
            prior: MAX_PROXY_CONTINUATIONS_PER_DISPATCH,
            max: MAX_PROXY_CONTINUATIONS_PER_DISPATCH,
        }
    );
    assert_eq!(
        fake.staff_calls().len(),
        usize::from(MAX_PROXY_CONTINUATIONS_PER_DISPATCH)
    );
    assert!(
        validate_proxy_continuation(&fourth, MAX_PROXY_CONTINUATIONS_PER_DISPATCH, "ok").is_err()
    );
}

#[test]
fn redacted_task_brief_formatting_enforces_size_markers_and_task_intent_guards() {
    let ok = format_redacted_task_with_brief(
        "Check migration parity in zeroclaw-infra",
        Some("Established decision: keep sessions.db append-only."),
    )
    .expect("valid redacted brief");
    assert!(ok.starts_with("[Task Context]\n"));
    assert!(ok.ends_with("Check migration parity in zeroclaw-infra"));

    let over = "x".repeat(MAX_TASK_CONTEXT_BRIEF_BYTES + 1);
    assert!(matches!(
        format_redacted_task_with_brief("Check parity", Some(&over)).unwrap_err(),
        TaskBriefError::BriefTooLarge { .. }
    ));
    assert!(matches!(
        format_redacted_task_with_brief("Check parity", Some("soul::identity")).unwrap_err(),
        TaskBriefError::ProtectedIdentityMarker { .. }
    ));
    assert!(matches!(
        format_redacted_task_with_brief("Check parity", Some("use --full-auto")).unwrap_err(),
        TaskBriefError::Admission(_)
    ));
}

/// Opt-in product-pair test. The external runner owns the isolated, disposable
/// worker and counts actual upstream mutations across fresh client processes.
#[tokio::test]
#[ignore = "requires the #725 isolated Tachi native fixture and loopback transport"]
async fn paired_native_staff_contract() {
    let env = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let endpoint = env("REI725_MCP_ENDPOINT");
    let uri: axum::http::Uri = endpoint.parse().expect("literal loopback MCP URI");
    assert_eq!(uri.scheme_str(), Some("http"));
    assert_eq!(uri.host(), Some("127.0.0.1"));
    assert_ne!(uri.port_u16().expect("explicit fixture port"), 6919);
    assert_eq!(uri.path_and_query().unwrap().as_str(), "/mcp");
    let workspace = std::path::PathBuf::from(env("REI725_WORKSPACE"));
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).expect("disposable fixture readme"),
        "dogfood fixture\n"
    );
    let dispatch = env("REI725_DISPATCH_ID");
    let expect_revision: u64 = env("REI725_EXPECT_REV").parse().expect("revision");
    let expect_turn: u64 = env("REI725_EXPECT_TURN").parse().expect("turn");
    let stage = env("REI725_STAGE");
    let dropped = env("REI725_EXPECT_DROPPED") == "1";
    let client = TachiStaffClient::from_config(
        &TachiConfig {
            enabled: true,
            endpoint,
            agent_identity: Some(env("REI725_AGENT_ID")),
            project: Some(env("REI725_PROJECT")),
            ..Default::default()
        },
        "paired-fixture",
    )
    .expect("fixture route");
    let read = client
        .status(&dispatch)
        .await
        .expect("canonical Staff status");
    assert_eq!(read.dispatch_id, dispatch);
    assert_eq!(read.revision, Some(expect_revision));
    if stage == "after_finish" {
        assert_eq!(read.state, RunState::Completed);
        assert!(read.is_terminal());
        assert_eq!(read.supervised_native_waiting_turn(), None);
        return;
    }
    assert_eq!(read.state, RunState::Working);
    assert_eq!(read.supervised_native_waiting_turn(), Some(expect_turn));
    let result = match stage.as_str() {
        "after_continue" => return,
        "continue" => client
            .continue_run(
                &dispatch,
                expect_revision,
                "Review the same disposable fixture once more.",
            )
            .await
            .map(|receipt| (receipt.dispatch_id, receipt.state, receipt.status_revision)),
        "finish" => client
            .finish_run(&dispatch, expect_revision)
            .await
            .map(|receipt| (receipt.dispatch_id, receipt.state, receipt.status_revision)),
        _ => panic!("unknown fixture stage: {stage}"),
    };
    if dropped {
        assert!(
            matches!(result, Err(TachiStaffError::SubmissionUnknown(_))),
            "{result:?}"
        );
    } else {
        let (receipt_dispatch, receipt_state, receipt_revision) =
            result.expect("the actual Tachi success receipt decodes");
        assert_eq!(receipt_dispatch, dispatch);
        assert_eq!(
            receipt_state,
            RunState::Working,
            "acceptance is not completion"
        );
        assert!(receipt_revision.is_some_and(|revision| revision > expect_revision));
    }
}
