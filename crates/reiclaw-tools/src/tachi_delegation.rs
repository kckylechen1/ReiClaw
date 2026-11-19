//! Thin L2 tools. Tachi owns execution truth; the existing session database
//! owns only local request admission and its canonical dispatch reference.
//! No owner/persona identity, memory, history, or raw execution authority is sent.
//! Protocol caller identity and routing metadata accompany the admitted task.

use crate::tachi_staff::{
    ContinueReceipt, MirrorCallSite, RunState, RunStatus, StaffRefs, StaffingReason,
    TachiStaffClient, TachiStaffError, nonterminal_status_for_dispatch, run_status_from_receipt,
};
use async_trait::async_trait;
use reiclaw_api::delegation_control::DelegationOperation;
use reiclaw_api::delegation_request::DelegationRequestClaim;
use reiclaw_api::tool::{Tool, ToolResult};
use reiclaw_config::tachi::TachiConfig;
use reiclaw_infra::session_sqlite::SqliteSessionBackend;
use reiclaw_policy::ToolOperation;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Runtime refusal code for changing the process-selected request ledger.
/// Live authorization/routing stays separate from this storage identity.
pub const STORAGE_ROOT_CHANGED: &str = "delegation_storage_root_changed";
/// Noninteractive callers must refuse a live policy requiring fresh approval.
pub const APPROVAL_REQUIRED: &str = "delegation_approval_required";
/// Maximum automatic proxy-continuation rounds per dispatch (ADR-025 §8).
pub const MAX_PROXY_CONTINUATIONS_PER_DISPATCH: u8 = 3;
/// Maximum UTF-8 byte length of an outbound redacted `[Task Context]` brief (ADR-025 §8).
pub const MAX_TASK_CONTEXT_BRIEF_BYTES: usize = 4_096;
/// Header prepended when an outbound task carries a redacted technical context brief.
pub const TASK_CONTEXT_HEADER: &str = "[Task Context]";

/// The runtime rechecks live agent/card policy and returns ephemeral config.
type DelegationResolver =
    Arc<dyn Fn(&str, ToolOperation) -> Result<(TachiConfig, PathBuf), String> + Send + Sync>;

/// Assemble the L2 tools against one runtime-owned authorization/config resolver.
/// This is the same control surface for any caller; it accepts no raw process authority.
pub fn tools(
    agent_alias: &str,
    resolve: impl Fn(&str, ToolOperation) -> Result<(TachiConfig, PathBuf), String>
    + Send
    + Sync
    + 'static,
) -> Vec<Arc<dyn Tool>> {
    let service = TachiDelegation::new(agent_alias, resolve);
    DelegationAction::ALL
        .into_iter()
        .map(|action| {
            Arc::new(TachiDelegationTool {
                service: service.clone(),
                action,
            }) as Arc<dyn Tool>
        })
        .collect()
}

/// The service shared by the registered tools. It holds transport policy
/// resolvers, not a second task registry. DB uniqueness also covers other
/// tool registries/processes and a restart between admission and the response.
pub struct TachiDelegation {
    agent_alias: String,
    resolve: DelegationResolver,
}

impl TachiDelegation {
    pub fn new(
        agent_alias: &str,
        resolve: impl Fn(&str, ToolOperation) -> Result<(TachiConfig, PathBuf), String>
        + Send
        + Sync
        + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent_alias: agent_alias.to_string(),
            resolve: Arc::new(resolve),
        })
    }

    /// The edge facade enters the exact same implementation as model tools.
    /// It cannot supply a dispatch ID, bypass claims, or choose process settings.
    pub async fn control(
        self: &Arc<Self>,
        operation: DelegationOperation,
        args: Value,
    ) -> anyhow::Result<ToolResult> {
        let action = match operation {
            DelegationOperation::Start => DelegationAction::Start,
            DelegationOperation::Status => DelegationAction::Status,
            DelegationOperation::Result => DelegationAction::Result,
            DelegationOperation::Cancel => DelegationAction::Cancel,
            DelegationOperation::List => return self.recent(args).await,
        };
        TachiDelegationTool {
            service: self.clone(),
            action,
        }
        .execute(args)
        .await
    }

    async fn recent(&self, args: Value) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            limit: u8,
        }
        let args: Args = match serde_json::from_value::<Args>(args) {
            Ok(args) if (1..=reiclaw_api::delegation_control::MAX_RECENT).contains(&args.limit) => {
                args
            }
            _ => return Ok(failure("invalid_arguments", &text("tool-tachi-invalid"))),
        };
        let (tachi, data_dir) = match (self.resolve)("tachi_status", ToolOperation::Read) {
            Ok(value) => value,
            Err(detail) => return Ok(resolver_failure(&detail)),
        };
        if let Err(error) = TachiStaffClient::from_config(&tachi, &self.agent_alias) {
            return Ok(staff_failure(error));
        }
        let route = route_fingerprint(&tachi, &self.agent_alias);
        let agent = self.agent_alias.clone();
        let limit = usize::from(args.limit);
        let rows = tokio::task::spawn_blocking(move || {
            SqliteSessionBackend::new(&data_dir)?.recent_delegation_bindings(&agent, limit + 1)
        })
        .await?;
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => return Ok(failure("storage_error", &error.to_string())),
        };
        let has_more = rows.len() > limit;
        let items: Vec<_> = rows
            .into_iter()
            .take(limit)
            .map(|row| {
                json!({
                    "request_id": row.request_id,
                    "submission": if row.dispatch_id.is_some() { "bound" } else { "unresolved" },
                    "dispatch_id": row.dispatch_id,
                    "route_matches": row.route_digest.as_deref() == Some(route.as_str()),
                })
            })
            .collect();
        Ok(ToolResult::ok(json!({"items":items,"has_more":has_more})))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DelegationAction {
    Start,
    Status,
    Result,
    Cancel,
    Watch,
}

impl DelegationAction {
    pub const ALL: [Self; 5] = [
        Self::Start,
        Self::Status,
        Self::Result,
        Self::Cancel,
        Self::Watch,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "tachi_start",
            Self::Status => "tachi_status",
            Self::Result => "tachi_result",
            Self::Cancel => "tachi_cancel",
            Self::Watch => "tachi_watch",
        }
    }
    fn operation(self) -> ToolOperation {
        match self {
            Self::Start | Self::Cancel => ToolOperation::Act,
            _ => ToolOperation::Read,
        }
    }
    fn description(self) -> &'static str {
        static STRINGS: OnceLock<[String; 5]> = OnceLock::new();
        let strings = STRINGS.get_or_init(|| {
            Self::ALL.map(|action| text(&format!("tool-{}", action.name().replace('_', "-"))))
        });
        &strings[self as usize]
    }
}

struct TachiDelegationTool {
    service: Arc<TachiDelegation>,
    action: DelegationAction,
}

reiclaw_api::tool_attribution!(
    TachiDelegationTool,
    reiclaw_api::attribution::ToolKind::Plugin
);

fn text(key: &str) -> String {
    crate::i18n::get_required_tool_string(key)
}

fn failure(code: &str, detail: &str) -> ToolResult {
    let detail = reiclaw_providers::sanitize_api_error(detail);
    let message =
        crate::i18n::get_required_tool_string_with_args("tool-tachi-error", &[("detail", &detail)]);
    ToolResult::partial(json!({"code": code}), message)
}

fn resolver_failure(detail: &str) -> ToolResult {
    match detail {
        STORAGE_ROOT_CHANGED => failure(
            "storage_root_changed",
            &text("tool-tachi-storage-root-changed"),
        ),
        APPROVAL_REQUIRED => failure("approval_required", &text("tool-tachi-denied")),
        _ => failure("denied", detail),
    }
}

fn staff_failure(error: TachiStaffError) -> ToolResult {
    let code = match &error {
        TachiStaffError::Unavailable(_) => "unavailable",
        TachiStaffError::SubmissionUnknown(_) => "submission_unknown",
        TachiStaffError::Refused(_) => "refused",
        TachiStaffError::UnknownHarness(_) => "unknown_harness",
        TachiStaffError::CancelUnsupported { .. } => "cancel_unsupported",
        TachiStaffError::StaleRevision { .. } => "stale_revision",
        TachiStaffError::Protocol(_) => "protocol_error",
    };
    failure(code, &error.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    request_id: String,
    harness: String,
    task: String,
    staffing_reason: StaffingReason,
    issue_ref: Option<String>,
    pr_ref: Option<String>,
    flow_id: Option<String>,
    #[serde(default)]
    context_brief: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    request_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    request_id: String,
    expected_status_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchArgs {
    request_id: String,
    max_wait_secs: u64,
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':'))
}
fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

/// The immutable route provenance fingerprint for a delegation claim: the
/// Tachi endpoint, the resolved caller identity and the project binding, as
/// of claim time. Status/result operations refuse a dispatch whose stored
/// digest differs from the CURRENT route's fingerprint; the #63 unattended
/// observer reuses this exact function for the same protection.
#[must_use]
pub fn route_fingerprint(tachi: &reiclaw_config::tachi::TachiConfig, agent_alias: &str) -> String {
    hash(&json!({
        "endpoint": tachi.endpoint.trim(),
        "identity": tachi.resolved_agent_identity(agent_alias),
        "project": tachi.project.as_deref().map(str::trim),
    }))
}

#[async_trait]
impl Tool for TachiDelegationTool {
    fn name(&self) -> &str {
        self.action.name()
    }
    fn description(&self) -> &str {
        self.action.description()
    }
    fn parameters_schema(&self) -> Value {
        let mut properties = json!({"request_id": {"type":"string", "description":text("tool-tachi-param-request-id"), "minLength":1, "maxLength":128}});
        let mut required = vec!["request_id"];
        match self.action {
            DelegationAction::Start => {
                properties["harness"] =
                    json!({"type":"string", "description":text("tool-tachi-param-harness")});
                properties["task"] = json!({"type":"string", "description":text("tool-tachi-param-task"), "minLength":1, "maxLength":16384});
                properties["staffing_reason"] = json!({"type":"string", "description":text("tool-tachi-param-reason"), "enum":StaffingReason::ALL.map(StaffingReason::as_str)});
                for key in ["issue_ref", "pr_ref", "flow_id"] {
                    properties[key] =
                        json!({"type":"string", "description":text("tool-tachi-param-ref")});
                }
                properties["context_brief"] = json!({
                    "type": "string",
                    "description": "Optional redacted technical context brief (max 4096 bytes). Never include Soul, User Model, credentials, or raw conversation history.",
                    "maxLength": MAX_TASK_CONTEXT_BRIEF_BYTES,
                });
                required.extend(["harness", "task", "staffing_reason"]);
            }
            DelegationAction::Cancel => {
                properties["expected_status_revision"] = json!({"type":"integer", "minimum":0, "description":text("tool-tachi-param-revision")});
                required.push("expected_status_revision");
            }
            DelegationAction::Watch => {
                properties["max_wait_secs"] = json!({"type":"integer", "minimum":1, "maximum":30, "description":text("tool-tachi-param-wait")});
                required.push("max_wait_secs");
            }
            _ => {}
        }
        json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let (tachi, data_dir) = match (self.service.resolve)(self.name(), self.action.operation()) {
            Ok(value) => value,
            Err(detail) => return Ok(resolver_failure(&detail)),
        };
        let client = match TachiStaffClient::from_config(&tachi, &self.service.agent_alias) {
            Ok(client) => client,
            Err(error) => return Ok(staff_failure(error)),
        };
        // Agent/request identity stays stable across routing changes. This
        // fingerprint becomes immutable provenance only when a claim is made;
        // live policy and routing are still resolved afresh on every operation.
        let scope = self.service.agent_alias.clone();
        let route_digest = route_fingerprint(&tachi, &self.service.agent_alias);
        let start: Option<StartArgs> = if self.action == DelegationAction::Start {
            match serde_json::from_value(args.clone()) {
                Ok(value) => Some(value),
                Err(error) => return Ok(failure("invalid_arguments", &error.to_string())),
            }
        } else {
            None
        };
        let request_id = match self.action {
            DelegationAction::Start => start.as_ref().map(|a| a.request_id.clone()),
            DelegationAction::Cancel => serde_json::from_value::<CancelArgs>(args.clone())
                .ok()
                .map(|a| a.request_id),
            DelegationAction::Watch => serde_json::from_value::<WatchArgs>(args.clone())
                .ok()
                .filter(|a| (1..=30).contains(&a.max_wait_secs))
                .map(|a| a.request_id),
            _ => serde_json::from_value::<ReadArgs>(args.clone())
                .ok()
                .map(|a| a.request_id),
        };
        let Some(request_id) = request_id.filter(|id| valid_request_id(id)) else {
            return Ok(failure("invalid_arguments", &text("tool-tachi-invalid")));
        };
        let mut outbound_task: Option<String> = None;
        if let Some(start) = &start {
            if start.task.trim().is_empty() || start.task.len() > 16384 {
                return Ok(failure("invalid_arguments", &text("tool-tachi-invalid")));
            }
            if let Err(error) = client.profile_for(&start.harness) {
                return Ok(staff_failure(error));
            }
            for value in [start.task.as_str(), request_id.as_str()]
                .into_iter()
                .chain(start.issue_ref.as_deref())
                .chain(start.pr_ref.as_deref())
                .chain(start.flow_id.as_deref())
            {
                if let Err(category) = crate::tachi_admission::scan_text(value) {
                    return Ok(failure("forbidden_content", &category.to_string()));
                }
            }
            let prepared =
                match prepare_outbound_task_with_brief(&start.task, start.context_brief.as_deref())
                {
                    Ok(task) => task,
                    Err(
                        error @ (TaskBriefError::EmptyTask | TaskBriefError::BriefTooLarge { .. }),
                    ) => {
                        return Ok(failure("invalid_arguments", &error.to_string()));
                    }
                    Err(
                        error @ (TaskBriefError::ProtectedIdentityMarker { .. }
                        | TaskBriefError::Admission(_)),
                    ) => {
                        return Ok(failure("forbidden_content", &error.to_string()));
                    }
                };
            outbound_task = Some(prepared);
        }
        let db = match tokio::task::spawn_blocking(move || SqliteSessionBackend::new(&data_dir))
            .await?
        {
            Ok(db) => Arc::new(db),
            Err(error) => return Ok(failure("storage_error", &error.to_string())),
        };
        if let Some(start) = start {
            let outbound_task = outbound_task.unwrap_or_else(|| start.task.clone());
            let digest =
                hash(&json!({"profile":client.profile_for(&start.harness)?, "arguments":args}));
            let claim = {
                let (db, scope, request_id, digest, route_digest) = (
                    db.clone(),
                    scope.clone(),
                    request_id.clone(),
                    digest.clone(),
                    route_digest.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    db.claim_delegation_request(&scope, &request_id, &digest, &route_digest)
                })
                .await?
            };
            match claim {
                Ok(DelegationRequestClaim::Created) => {}
                Ok(DelegationRequestClaim::Existing {
                    dispatch_id: Some(id),
                }) => {
                    client
                        .capture_staff_mirror_experience(
                            &id,
                            &nonterminal_status_for_dispatch(&id),
                            MirrorCallSite::BoundReplay,
                        )
                        .await;
                    return Ok(ToolResult::ok(
                        json!({"request_id":request_id,"dispatch_id":id,"accepted":true,"replayed":true}),
                    ));
                }
                Ok(DelegationRequestClaim::Existing { dispatch_id: None }) => {
                    return Ok(failure(
                        "submission_unresolved",
                        &text("tool-tachi-unresolved"),
                    ));
                }
                Ok(DelegationRequestClaim::Conflict) => {
                    return Ok(failure("request_conflict", &text("tool-tachi-conflict")));
                }
                Err(error) => return Ok(failure("storage_error", &error.to_string())),
            }
            let refs = StaffRefs {
                issue_ref: start.issue_ref,
                pr_ref: start.pr_ref,
                flow_id: start.flow_id,
            };
            let receipt = match client
                .start(&start.harness, &outbound_task, start.staffing_reason, &refs)
                .await
            {
                Ok(receipt) => receipt,
                Err(error @ TachiStaffError::Unavailable(_)) => {
                    // start's Unavailable contract proves tools/call was never
                    // sent. Other failures never release a durable claim.
                    let release = tokio::task::spawn_blocking(move || {
                        db.release_unsent_delegation_request(
                            &scope,
                            &request_id,
                            &digest,
                            &route_digest,
                        )
                    })
                    .await?;
                    if let Err(storage) = release {
                        return Ok(failure("storage_error", &storage.to_string()));
                    }
                    return Ok(staff_failure(error));
                }
                Err(error) => return Ok(staff_failure(error)),
            };
            let id = receipt.dispatch_id.clone();
            let output_request_id = request_id.clone();
            let bind_id = id.clone();
            let stored = tokio::task::spawn_blocking(move || {
                db.bind_delegation_request(&scope, &request_id, &digest, &route_digest, &bind_id)
            })
            .await?;
            if let Err(error) = stored {
                // Binding failure keeps today's partial receipt; do not conceal
                // it with mirror work or attempt register repair here.
                return Ok(ToolResult::partial(
                    json!({"code":"binding_write_failed", "request_id":output_request_id, "receipt":receipt}),
                    crate::i18n::get_required_tool_string_with_args(
                        "tool-tachi-binding-failed",
                        &[(
                            "detail",
                            &reiclaw_providers::sanitize_api_error(&error.to_string()),
                        )],
                    ),
                ));
            }
            client
                .capture_staff_mirror_experience(
                    &id,
                    &run_status_from_receipt(&receipt),
                    MirrorCallSite::StartBind,
                )
                .await;
            return Ok(ToolResult::ok(
                json!({"request_id":output_request_id,"accepted":true,"receipt":receipt,"replayed":false}),
            ));
        }
        let binding = {
            let (request_id, scope) = (request_id.clone(), scope.clone());
            tokio::task::spawn_blocking(move || db.read_delegation_request(&scope, &request_id))
                .await?
        };
        let dispatch_id = match binding {
            Ok(Some(binding)) if binding.route_digest.as_deref() != Some(route_digest.as_str()) => {
                return Ok(failure(
                    "route_conflict",
                    &text("tool-tachi-route-conflict"),
                ));
            }
            Ok(Some(binding)) => match binding.dispatch_id {
                Some(id) => id,
                None => {
                    return Ok(failure(
                        "submission_unresolved",
                        &text("tool-tachi-unresolved"),
                    ));
                }
            },
            Ok(None) => return Ok(failure("unknown_request", &text("tool-tachi-unknown"))),
            Err(error) => return Ok(failure("storage_error", &error.to_string())),
        };
        let output = match self.action {
            DelegationAction::Status => match client.status(&dispatch_id).await {
                Ok(status) => {
                    client
                        .capture_staff_mirror_experience(
                            &dispatch_id,
                            &status,
                            MirrorCallSite::Status,
                        )
                        .await;
                    Ok(json!({"request_id":request_id,"status":status}))
                }
                Err(error) => Err(error),
            },
            DelegationAction::Result => match client.result(&dispatch_id).await {
                Ok(mut result) => {
                    client
                        .capture_staff_mirror_experience(
                            &dispatch_id,
                            &result.status,
                            MirrorCallSite::Result,
                        )
                        .await;
                    result.body = result
                        .body
                        .map(|body| reiclaw_providers::scrub_secret_patterns(&body));
                    Ok(
                        json!({"request_id":request_id,"result":result,"trust":"untrusted_external_report","accepted_by_body":false}),
                    )
                }
                Err(error) => Err(error),
            },
            DelegationAction::Cancel => {
                let cancel: CancelArgs = serde_json::from_value(args)?;
                match client
                    .cancel(&dispatch_id, cancel.expected_status_revision)
                    .await
                {
                    Ok(receipt) => {
                        // Cancel receipt is not a RunStatus; re-read canonical
                        // status then share the same capture helper.
                        match client.status(&dispatch_id).await {
                            Ok(status) => {
                                client
                                    .capture_staff_mirror_experience(
                                        &dispatch_id,
                                        &status,
                                        MirrorCallSite::CancelReread,
                                    )
                                    .await;
                            }
                            Err(error) => {
                                ::reiclaw_log::record!(
                                    WARN,
                                    ::reiclaw_log::Event::new(
                                        module_path!(),
                                        ::reiclaw_log::Action::Fail,
                                    )
                                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                                    .with_attrs(json!({
                                        "mirror": true,
                                        "site": "cancel_reread",
                                        "op": "capture_gap",
                                        "dispatch_id": dispatch_id,
                                        "error": reiclaw_providers::sanitize_api_error(&error.to_string()),
                                    })),
                                    "tachi_staff: mirror capture gap after cancel (status re-read failed)"
                                );
                            }
                        }
                        Ok(
                            json!({"request_id":request_id,"dispatch_id":dispatch_id,"receipt":receipt}),
                        )
                    }
                    Err(error) => Err(error),
                }
            }
            DelegationAction::Watch => {
                let watch: WatchArgs = serde_json::from_value(args)?;
                match client
                    .wait_until_terminal(&dispatch_id, Duration::from_secs(watch.max_wait_secs))
                    .await
                {
                    Ok(status) => {
                        // Watch timeout while still running is nonterminal: helper
                        // ensures register only and does not observe.
                        client
                            .capture_staff_mirror_experience(
                                &dispatch_id,
                                &status,
                                MirrorCallSite::Watch,
                            )
                            .await;
                        Ok(
                            json!({"request_id":request_id,"terminal":status.is_terminal(),"status":status,"watch":"bounded_polling"}),
                        )
                    }
                    Err(error) => Err(error),
                }
            }
            DelegationAction::Start => Err(TachiStaffError::Protocol(
                "start reached read path".to_string(),
            )),
        };
        Ok(match output {
            Ok(value) => ToolResult::ok(value),
            Err(error) => staff_failure(error),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// ADR-025 §8 proxy-continuation gates and outbound redacted briefs
// ─────────────────────────────────────────────────────────────────────────

const PROTECTED_IDENTITY_MARKERS: &[&str] = &[
    "soul::",
    "[agentsoul]",
    "## identity",
    "## who i've become",
    "## principles",
    "## our story so far",
    "## owner profile",
    "[user model]",
    "## user model",
    "user_model",
    "soul.db",
    "user_model.db",
];

/// Return the first protected Soul or User Model marker found in `text`, if any.
#[must_use]
pub fn find_protected_identity_marker(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    PROTECTED_IDENTITY_MARKERS
        .iter()
        .copied()
        .find(|marker| lower.contains(marker))
}

fn value_has_auth_required(value: &Value, depth: u8) -> bool {
    if depth == 0 {
        return false;
    }
    match value {
        Value::String(raw) => raw.to_ascii_lowercase().contains("auth_required"),
        Value::Array(items) => items
            .iter()
            .any(|item| value_has_auth_required(item, depth - 1)),
        Value::Object(map) => {
            if map.get("auth_required").and_then(Value::as_bool) == Some(true) {
                return true;
            }
            map.values()
                .any(|item| value_has_auth_required(item, depth - 1))
        }
        _ => false,
    }
}

fn status_indicates_auth_required(status: &RunStatus) -> bool {
    if let RunState::Other(raw) = &status.state
        && raw.to_ascii_lowercase().contains("auth_required")
    {
        return true;
    }
    if status
        .closure_kind
        .as_deref()
        .is_some_and(|kind| kind.to_ascii_lowercase().contains("auth_required"))
    {
        return true;
    }
    status
        .read_projection
        .as_ref()
        .is_some_and(|projection| value_has_auth_required(projection, 8))
}

/// Refusal reasons for automatic proxy continuation (`tachi_staff(action = "continue")`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyContinuationError {
    /// `AUTH_REQUIRED` states must never be auto-answered by the body.
    #[error("proxy continuation refused: AUTH_REQUIRED must be handled by the owner")]
    AuthRequired,
    /// Run is closed as `INPUT_REQUIRED` + `partial` (terminal).
    #[error("proxy continuation refused: run is terminally closed (closure_kind=partial)")]
    PartialClosure,
    /// Run is in a state other than `TASK_STATE_INPUT_REQUIRED`.
    #[error("proxy continuation refused: state {state} is not TASK_STATE_INPUT_REQUIRED")]
    NotInputRequired { state: RunState },
    /// Per-dispatch automatic proxy-continuation cap reached.
    #[error("proxy continuation refused: automatic continuation cap reached ({prior}/{max})")]
    CapExceeded { prior: u8, max: u8 },
    /// Canonical status did not include a `status_revision` to quote on `continue`.
    #[error("proxy continuation refused: missing canonical status_revision")]
    MissingRevision,
    /// Empty or whitespace-only answer.
    #[error("proxy continuation refused: answer must not be empty")]
    EmptyAnswer,
    /// Answer violates outbound task admission rules or carries protected Soul/UserModel markers.
    #[error("proxy continuation refused: {0}")]
    ForbiddenContent(String),
    /// Underlying `tachi_staff` client error during `continue_run`.
    #[error(transparent)]
    Staff(#[from] TachiStaffError),
}

fn validate_proxy_continuation_status(
    status: &RunStatus,
    prior_auto_continuations: u8,
) -> Result<(), ProxyContinuationError> {
    if status_indicates_auth_required(status) {
        return Err(ProxyContinuationError::AuthRequired);
    }
    if status.state != RunState::InputRequired {
        return Err(ProxyContinuationError::NotInputRequired {
            state: status.state.clone(),
        });
    }
    if status
        .closure_kind
        .as_deref()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("partial"))
    {
        return Err(ProxyContinuationError::PartialClosure);
    }
    if prior_auto_continuations >= MAX_PROXY_CONTINUATIONS_PER_DISPATCH {
        return Err(ProxyContinuationError::CapExceeded {
            prior: prior_auto_continuations,
            max: MAX_PROXY_CONTINUATIONS_PER_DISPATCH,
        });
    }
    Ok(())
}

/// Validate whether automatic proxy continuation is permitted for a verified
/// canonical [`RunStatus`], current auto-continuation count, and proposed `answer`.
///
/// Enforces ADR-025 §8:
/// - refuses `AUTH_REQUIRED` (in `state`, `closure_kind`, or `read_projection`);
/// - allows `continue` ONLY when `state == RunState::InputRequired` and
///   `closure_kind != Some("partial")`;
/// - refuses once `prior_auto_continuations >= MAX_PROXY_CONTINUATIONS_PER_DISPATCH`;
/// - rejects empty/whitespace `answer`, forbidden execution/credential content,
///   and Soul/UserModel markers.
pub fn validate_proxy_continuation(
    status: &RunStatus,
    prior_auto_continuations: u8,
    answer: &str,
) -> Result<(), ProxyContinuationError> {
    validate_proxy_continuation_status(status, prior_auto_continuations)?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Err(ProxyContinuationError::EmptyAnswer);
    }
    if let Some(marker) = find_protected_identity_marker(answer) {
        return Err(ProxyContinuationError::ForbiddenContent(format!(
            "protected identity marker `{marker}`"
        )));
    }
    if let Err(category) = crate::tachi_admission::scan_text(answer) {
        return Err(ProxyContinuationError::ForbiddenContent(
            category.to_string(),
        ));
    }
    Ok(())
}

/// Stateful per-dispatch gate enforcing ADR-025 §8 proxy-continuation safety
/// and loop caps before invoking [`TachiStaffClient::continue_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyContinuationGate {
    dispatch_id: String,
    prior_auto_continuations: u8,
    max_auto_continuations: u8,
}

impl ProxyContinuationGate {
    /// Create a fresh gate for `dispatch_id` capped at [`MAX_PROXY_CONTINUATIONS_PER_DISPATCH`].
    #[must_use]
    pub fn new(dispatch_id: impl Into<String>) -> Self {
        Self::with_prior_count(dispatch_id, 0)
    }

    /// Create a gate initialized with `prior_auto_continuations` already consumed.
    #[must_use]
    pub fn with_prior_count(dispatch_id: impl Into<String>, prior_auto_continuations: u8) -> Self {
        Self {
            dispatch_id: dispatch_id.into(),
            prior_auto_continuations,
            max_auto_continuations: MAX_PROXY_CONTINUATIONS_PER_DISPATCH,
        }
    }

    /// Canonical dispatch id bound to this gate.
    #[must_use]
    pub fn dispatch_id(&self) -> &str {
        &self.dispatch_id
    }

    /// Number of automatic continuations already consumed for this dispatch.
    #[must_use]
    pub const fn prior_auto_continuations(&self) -> u8 {
        self.prior_auto_continuations
    }

    /// Remaining automatic continuations allowed for this dispatch.
    #[must_use]
    pub const fn remaining_auto_continuations(&self) -> u8 {
        self.max_auto_continuations
            .saturating_sub(self.prior_auto_continuations)
    }

    /// Pre-flight check of `status` and loop cap before resolving an answer.
    /// Returns the canonical `status_revision` when proxy continuation is eligible.
    pub fn evaluate(&self, status: &RunStatus) -> Result<u64, ProxyContinuationError> {
        if status.dispatch_id != self.dispatch_id {
            return Err(TachiStaffError::Protocol("continue dispatch mismatch".to_string()).into());
        }
        validate_proxy_continuation_status(status, self.prior_auto_continuations)?;
        status
            .revision
            .ok_or(ProxyContinuationError::MissingRevision)
    }

    /// Verify all proxy-continuation invariants for `status` and `answer`,
    /// returning the canonical `status_revision` to quote on `continue`.
    pub fn admit(&self, status: &RunStatus, answer: &str) -> Result<u64, ProxyContinuationError> {
        if status.dispatch_id != self.dispatch_id {
            return Err(TachiStaffError::Protocol("continue dispatch mismatch".to_string()).into());
        }
        validate_proxy_continuation(status, self.prior_auto_continuations, answer)?;
        status
            .revision
            .ok_or(ProxyContinuationError::MissingRevision)
    }

    /// Admit and send `tachi_staff(action = "continue")` through `client`.
    ///
    /// Increments the consumed continuation count when `continue_run` is
    /// accepted or returns `SubmissionUnknown` (transmitted mutation).
    pub async fn continue_run(
        &mut self,
        client: &TachiStaffClient,
        status: &RunStatus,
        answer: &str,
    ) -> Result<ContinueReceipt, ProxyContinuationError> {
        let expected_revision = self.admit(status, answer)?;
        let result = client
            .continue_run(&status.dispatch_id, expected_revision, answer.trim())
            .await;
        if result.is_ok() || matches!(result, Err(TachiStaffError::SubmissionUnknown(_))) {
            self.prior_auto_continuations = self.prior_auto_continuations.saturating_add(1);
        }
        result.map_err(ProxyContinuationError::from)
    }
}

/// Error returned when composing an outbound task with a redacted `[Task Context]` brief.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskBriefError {
    /// Task text is empty or whitespace-only.
    #[error("task must not be empty")]
    EmptyTask,
    /// Context brief exceeds [`MAX_TASK_CONTEXT_BRIEF_BYTES`].
    #[error("context brief exceeds {max} byte limit (got {actual} bytes)")]
    BriefTooLarge { actual: usize, max: usize },
    /// Protected Soul or User Model marker detected; fail closed.
    #[error("forbidden protected identity marker in outbound text: {marker}")]
    ProtectedIdentityMarker { marker: &'static str },
    /// Existing `TaskIntent` / `tachi_admission` text guard refused the text.
    #[error("forbidden outbound task content: {0}")]
    Admission(#[from] crate::tachi_admission::ForbiddenCategory),
}

/// Compose an outbound delegated task, optionally prepending a bounded
/// `[Task Context]` brief derived from technical checkpoint/memory facts.
///
/// Enforces ADR-017 §8 and ADR-025 §8 fail-closed:
/// - rejects empty `task`;
/// - bounds `context_brief` to `<= 4_096` bytes;
/// - rejects any Soul/UserModel markers (`soul::`, `[AgentSoul]`, `## Identity`,
///   `## Who I've become`, `user_model`, etc.) in either `task` or `context_brief`;
/// - validates `task`, `context_brief`, and the combined payload through
///   [`crate::tachi_admission::scan_text`] so CLI flags, worktrees, SSH/tmux
///   overrides, and credential patterns stay rejected.
pub fn format_redacted_task_with_brief(
    task: &str,
    context_brief: Option<&str>,
) -> Result<String, TaskBriefError> {
    let task = task.trim();
    if task.is_empty() {
        return Err(TaskBriefError::EmptyTask);
    }
    if let Some(marker) = find_protected_identity_marker(task) {
        return Err(TaskBriefError::ProtectedIdentityMarker { marker });
    }
    crate::tachi_admission::scan_text(task)?;

    let Some(raw_brief) = context_brief else {
        return Ok(task.to_string());
    };
    if raw_brief.len() > MAX_TASK_CONTEXT_BRIEF_BYTES {
        return Err(TaskBriefError::BriefTooLarge {
            actual: raw_brief.len(),
            max: MAX_TASK_CONTEXT_BRIEF_BYTES,
        });
    }
    let brief = raw_brief.trim();
    if brief.is_empty() {
        return Ok(task.to_string());
    }
    if let Some(marker) = find_protected_identity_marker(brief) {
        return Err(TaskBriefError::ProtectedIdentityMarker { marker });
    }
    crate::tachi_admission::scan_text(brief)?;

    let combined = format!("{TASK_CONTEXT_HEADER}\n{brief}\n\n{task}");
    crate::tachi_admission::scan_text(&combined)?;
    Ok(combined)
}

/// Scrub credential patterns from an optional caller-supplied context brief and
/// format the outbound delegated task with `[Task Context]`.
///
/// Checks `MAX_TASK_CONTEXT_BRIEF_BYTES` and protected Soul/UserModel markers
/// on the raw brief before redacting secret tokens via
/// [`reiclaw_providers::scrub_secret_patterns`], then enforces
/// [`format_redacted_task_with_brief`]. Never reads `soul.db` or `user_model.db`.
pub fn prepare_outbound_task_with_brief(
    task: &str,
    context_brief: Option<&str>,
) -> Result<String, TaskBriefError> {
    let Some(raw_brief) = context_brief else {
        return format_redacted_task_with_brief(task, None);
    };
    if raw_brief.len() > MAX_TASK_CONTEXT_BRIEF_BYTES {
        return Err(TaskBriefError::BriefTooLarge {
            actual: raw_brief.len(),
            max: MAX_TASK_CONTEXT_BRIEF_BYTES,
        });
    }
    if let Some(marker) = find_protected_identity_marker(raw_brief) {
        return Err(TaskBriefError::ProtectedIdentityMarker { marker });
    }
    let scrubbed = reiclaw_providers::scrub_secret_patterns(raw_brief);
    format_redacted_task_with_brief(task, Some(&scrubbed))
}

#[cfg(test)]
mod continuity_tests {
    use super::*;

    fn status_with(
        state: RunState,
        closure_kind: Option<&str>,
        revision: Option<u64>,
    ) -> RunStatus {
        RunStatus {
            dispatch_id: "d-continuity".to_string(),
            state,
            revision,
            closure_kind: closure_kind.map(str::to_string),
            result_written: None,
            updated_at: None,
            read_projection: None,
        }
    }

    #[test]
    fn proxy_continuation_cap_constant_matches_adr_025() {
        assert_eq!(MAX_PROXY_CONTINUATIONS_PER_DISPATCH, 3);
    }

    #[test]
    fn validate_proxy_continuation_allows_only_non_partial_input_required_under_cap() {
        let ready = status_with(RunState::InputRequired, None, Some(4));
        assert!(validate_proxy_continuation(&ready, 0, "Use snake_case columns").is_ok());
        assert!(validate_proxy_continuation(&ready, 2, "Keep the current API").is_ok());
        assert_eq!(
            validate_proxy_continuation(&ready, 3, "Fourth round").unwrap_err(),
            ProxyContinuationError::CapExceeded { prior: 3, max: 3 }
        );
    }

    #[test]
    fn validate_proxy_continuation_refuses_auth_required_partial_other_states_and_empty() {
        let auth_state = status_with(
            RunState::Other("TASK_STATE_AUTH_REQUIRED".into()),
            None,
            Some(1),
        );
        assert_eq!(
            validate_proxy_continuation(&auth_state, 0, "proceed").unwrap_err(),
            ProxyContinuationError::AuthRequired
        );

        let mut auth_proj = status_with(RunState::InputRequired, None, Some(1));
        auth_proj.read_projection = Some(json!({"auth_required": true}));
        assert_eq!(
            validate_proxy_continuation(&auth_proj, 0, "proceed").unwrap_err(),
            ProxyContinuationError::AuthRequired
        );

        let partial = status_with(RunState::InputRequired, Some("partial"), Some(1));
        assert_eq!(
            validate_proxy_continuation(&partial, 0, "proceed").unwrap_err(),
            ProxyContinuationError::PartialClosure
        );

        for state in [
            RunState::Submitted,
            RunState::Working,
            RunState::Completed,
            RunState::Failed,
            RunState::Canceled,
        ] {
            let st = status_with(state.clone(), None, Some(1));
            assert_eq!(
                validate_proxy_continuation(&st, 0, "proceed").unwrap_err(),
                ProxyContinuationError::NotInputRequired { state }
            );
        }

        let ready = status_with(RunState::InputRequired, None, Some(1));
        assert_eq!(
            validate_proxy_continuation(&ready, 0, "   ").unwrap_err(),
            ProxyContinuationError::EmptyAnswer
        );
        assert!(matches!(
            validate_proxy_continuation(&ready, 0, "run --full-auto").unwrap_err(),
            ProxyContinuationError::ForbiddenContent(_)
        ));
    }

    #[test]
    fn format_redacted_task_with_brief_prepends_context_and_enforces_bounds_and_markers() {
        assert_eq!(
            format_redacted_task_with_brief("Review the SQLite schema", None).unwrap(),
            "Review the SQLite schema"
        );
        assert_eq!(
            format_redacted_task_with_brief("Review the SQLite schema", Some("   ")).unwrap(),
            "Review the SQLite schema"
        );
        let formatted = format_redacted_task_with_brief(
            "Review the SQLite schema",
            Some("Decision: preserve existing sessions table columns."),
        )
        .unwrap();
        assert_eq!(
            formatted,
            "[Task Context]\nDecision: preserve existing sessions table columns.\n\nReview the SQLite schema"
        );

        let at_limit = "a".repeat(MAX_TASK_CONTEXT_BRIEF_BYTES);
        assert!(format_redacted_task_with_brief("Review schema", Some(&at_limit)).is_ok());

        let over_limit = "a".repeat(MAX_TASK_CONTEXT_BRIEF_BYTES + 1);
        assert_eq!(
            format_redacted_task_with_brief("Review schema", Some(&over_limit)).unwrap_err(),
            TaskBriefError::BriefTooLarge {
                actual: MAX_TASK_CONTEXT_BRIEF_BYTES + 1,
                max: MAX_TASK_CONTEXT_BRIEF_BYTES,
            }
        );

        for forbidden_brief in [
            "soul::identity::tone",
            "[AgentSoul] persona summary",
            "## Identity\nAgent persona",
            "## Who I've become\nGrowth",
            "Loaded from user_model table",
        ] {
            assert!(
                matches!(
                    format_redacted_task_with_brief("Review schema", Some(forbidden_brief))
                        .unwrap_err(),
                    TaskBriefError::ProtectedIdentityMarker { .. }
                ),
                "expected protected marker rejection for `{forbidden_brief}`"
            );
        }

        for forbidden_text in [
            "pass --full-auto to the runner",
            "use /Users/zeroclaw_user/worktrees/slice",
            "connect over ssh to build host",
        ] {
            assert!(
                matches!(
                    format_redacted_task_with_brief("Review schema", Some(forbidden_text))
                        .unwrap_err(),
                    TaskBriefError::Admission(_)
                ),
                "expected admission rejection for brief `{forbidden_text}`"
            );
            assert!(
                matches!(
                    format_redacted_task_with_brief(forbidden_text, Some("Valid context"))
                        .unwrap_err(),
                    TaskBriefError::Admission(_)
                ),
                "expected admission rejection for task `{forbidden_text}`"
            );
        }
    }

    #[test]
    fn prepare_outbound_task_with_brief_scrubs_secrets_and_blocks_soul_and_user_model() {
        let prepared = prepare_outbound_task_with_brief(
            "Check gateway handler",
            Some(
                "Using callback https://example.test/cb?token=supersecret and key sk-ant-secret123",
            ),
        )
        .unwrap();
        assert!(prepared.starts_with("[Task Context]\n"));
        assert!(prepared.contains("[REDACTED]"));
        assert!(!prepared.contains("supersecret"));
        assert!(!prepared.contains("sk-ant-secret123"));

        for bad_brief in [
            "Read soul.db before answering",
            "Check user_model.db preferences",
            "api_key=raw-secret-value",
            "-----BEGIN PRIVATE KEY-----\nabc",
        ] {
            assert!(
                prepare_outbound_task_with_brief("Check handler", Some(bad_brief)).is_err(),
                "expected rejection for `{bad_brief}`"
            );
        }

        let over = "x".repeat(MAX_TASK_CONTEXT_BRIEF_BYTES + 1);
        assert!(matches!(
            prepare_outbound_task_with_brief("Check handler", Some(&over)),
            Err(TaskBriefError::BriefTooLarge { .. })
        ));
    }

    #[test]
    fn proxy_continuation_gate_evaluate_checks_status_cap_and_revision_before_answer() {
        let gate = ProxyContinuationGate::new("d-continuity");
        assert_eq!(
            gate.evaluate(&status_with(RunState::InputRequired, None, Some(7)))
                .unwrap(),
            7
        );
        assert_eq!(
            gate.evaluate(&status_with(RunState::InputRequired, None, None))
                .unwrap_err(),
            ProxyContinuationError::MissingRevision
        );
        assert_eq!(
            gate.evaluate(&status_with(
                RunState::Other("TASK_STATE_AUTH_REQUIRED".into()),
                None,
                Some(7)
            ))
            .unwrap_err(),
            ProxyContinuationError::AuthRequired
        );
        let capped = ProxyContinuationGate::with_prior_count(
            "d-continuity",
            MAX_PROXY_CONTINUATIONS_PER_DISPATCH,
        );
        assert_eq!(
            capped
                .evaluate(&status_with(RunState::InputRequired, None, Some(7)))
                .unwrap_err(),
            ProxyContinuationError::CapExceeded {
                prior: MAX_PROXY_CONTINUATIONS_PER_DISPATCH,
                max: MAX_PROXY_CONTINUATIONS_PER_DISPATCH,
            }
        );
    }

    #[test]
    fn tachi_delegation_never_reads_soul_or_user_model_stores() {
        let source = include_str!("tachi_delegation.rs");
        let prod = source
            .split("#[cfg(test)]")
            .next()
            .expect("prod section before cfg(test)");
        for forbidden in [
            "SoulProfileStore",
            "UserModelStore",
            "soul_profile",
            "SOUL.md",
        ] {
            assert!(
                !prod.contains(forbidden),
                "tachi_delegation production code must not reference `{forbidden}`"
            );
        }
    }
}
