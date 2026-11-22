//! Unified owner review view. Soul and User Model stores remain canonical;
//! review actions use their existing operator-gated endpoints.
//! Collaborative DailyDigest edits and sleep action-candidate confirmations
//! persist to `memorycore.db` via the gateway's `Memory` handle, while
//! explicit candidate dispatch claims are owned by `sessions.db` and
//! external execution remains in Tachi.

use crate::AppState;
use crate::operator_auth::OperatorAuth;
use axum::{
    Extension, Router,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use reiclaw_api::delegation_control::DelegationOperation;
use reiclaw_api::memory_traits::{
    ActionCandidateStatus, DailyDigest, DailyDigestCasResult, validate_agent_id,
    validate_candidate_id, validate_non_soul_key,
};
use reiclaw_config::schema::Config;
use reiclaw_config::tachi::TachiConfig;
use reiclaw_infra::session_sqlite::{
    CandidateDispatchClaim, CandidateDispatchClaimInput, CandidateDispatchRecord,
    CandidateDispatchState, SqliteSessionBackend,
};
use reiclaw_memory::companion::{SoulProfileStore, UserModelStore};
use reiclaw_policy::{PerSenderTracker, SecurityPolicy, ToolOperation};
use reiclaw_tools::tachi_delegation::{
    TachiDelegation, TaskBriefError, prepare_outbound_task_with_brief, route_fingerprint,
    valid_request_id,
};
use reiclaw_tools::tachi_staff::{TachiStaffClient, TachiStaffError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const DAILY_DIGEST_INBOX_LIMIT: usize = 14;
const DAILY_DIGEST_LOOKUP_LIMIT: usize = 60;
const DISPATCH_BODY_LIMIT: usize = 24 * 1024;
const DISPATCH_CONTROL_TIMEOUT: Duration = Duration::from_secs(125);
const MAX_TASK_BYTES: usize = 16_384;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/review/inbox", get(inbox))
        .route(
            "/api/review/daily-digest/{agent}/{day_key}",
            put(update_daily_digest),
        )
        .route(
            "/api/review/action-candidates/{agent}/{day_key}/resolve",
            post(resolve_action_candidate),
        )
}

pub(crate) fn dispatch_routes(state: AppState) -> Router {
    dispatch_routes_with_timeout(state, DISPATCH_CONTROL_TIMEOUT)
}

pub(crate) fn dispatch_routes_with_timeout(state: AppState, timeout: Duration) -> Router {
    let context = Arc::new(DispatchContext {
        selected_data_dir: state.config.read().data_dir.clone(),
        tracker: PerSenderTracker::new(),
        slots: Arc::new(tokio::sync::Semaphore::new(8)),
        timeout,
        sessions_db: tokio::sync::OnceCell::new(),
    });
    Router::new()
        .route(
            "/api/review/action-candidates/{agent}/{day_key}/dispatch",
            post(dispatch_action_candidate),
        )
        .layer(Extension(context))
        .with_state(state)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            DISPATCH_BODY_LIMIT,
        ))
        // Tachi start may outlive the ordinary 30s gateway timeout.
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout.saturating_add(Duration::from_secs(5)),
        ))
}

struct DispatchContext {
    selected_data_dir: PathBuf,
    tracker: PerSenderTracker,
    slots: Arc<tokio::sync::Semaphore>,
    timeout: Duration,
    sessions_db: tokio::sync::OnceCell<Arc<SqliteSessionBackend>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxQuery {
    agent: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}
fn default_limit() -> usize {
    100
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, axum::Json(json!({"code": code, "error": message}))).into_response()
}

fn revision_conflict(current_revision: u64) -> Response {
    (
        StatusCode::CONFLICT,
        axum::Json(json!({
            "code": "revision_conflict",
            "error": format!("daily digest revision conflict; current revision is {current_revision}"),
            "current_revision": current_revision,
        })),
    )
        .into_response()
}

fn rfc3339_to_unix(ts: &str) -> u64 {
    reiclaw_api::time::parse_rfc3339(ts)
        .map(|t| u64::try_from(t.as_second().max(0)).unwrap_or(0))
        .unwrap_or(0)
}

fn clean_string_list(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn sha256_json(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

fn validate_agent_and_day(
    state: &AppState,
    agent: &str,
    day_key: &str,
    require_enabled: bool,
) -> Result<(), Box<Response>> {
    {
        let config = state.config.read();
        let Some(agent_cfg) = config.agent(agent) else {
            return Err(Box::new(error(
                StatusCode::NOT_FOUND,
                "unknown_agent",
                "unknown configured agent",
            )));
        };
        if require_enabled && !agent_cfg.enabled {
            return Err(Box::new(error(
                StatusCode::FORBIDDEN,
                "denied",
                "delegation denied by current agent policy",
            )));
        }
    }
    if let Err(err) = validate_agent_id(agent) {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "invalid",
            &err.to_string(),
        )));
    }
    if let Err(err) = validate_non_soul_key(day_key, "day_key") {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "invalid",
            &err.to_string(),
        )));
    }
    Ok(())
}

async fn load_daily_digest(
    state: &AppState,
    agent: &str,
    day_key: &str,
) -> Result<DailyDigest, Response> {
    let digests = state
        .mem
        .load_daily_digests(agent, DAILY_DIGEST_LOOKUP_LIMIT)
        .await
        .map_err(|err| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "store_unavailable",
                &err.to_string(),
            )
        })?;
    digests
        .into_iter()
        .find(|d| d.day_key == day_key)
        .ok_or_else(|| {
            error(
                StatusCode::NOT_FOUND,
                "daily_digest_not_found",
                "daily digest not found",
            )
        })
}

fn dispatch_projection_json(rec: &CandidateDispatchRecord) -> Value {
    json!({
        "candidate_id": rec.candidate_id,
        "source_revision": rec.source_revision,
        "request_id": rec.request_id,
        "harness": rec.harness,
        "state": rec.state,
        "dispatch_id": rec.dispatch_id,
        "created_at": rec.created_at,
        "updated_at": rec.updated_at,
    })
}

fn build_action_dispatches_map<'a>(
    dispatches: impl IntoIterator<Item = &'a CandidateDispatchRecord>,
) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for rec in dispatches {
        map.insert(rec.candidate_id.clone(), dispatch_projection_json(rec));
    }
    map
}

fn project_digest_json(
    digest: &DailyDigest,
    dispatches: &[CandidateDispatchRecord],
) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(digest)?;
    let normalized = digest.normalized_candidates();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("candidate_items".into(), serde_json::to_value(&normalized)?);
        obj.insert(
            "action_dispatches".into(),
            Value::Object(build_action_dispatches_map(dispatches)),
        );
    }
    Ok(value)
}

async fn load_day_dispatches(
    data_dir: PathBuf,
    agent: String,
    day_key: String,
) -> Result<Vec<CandidateDispatchRecord>, Response> {
    match tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let sessions = SqliteSessionBackend::new(&data_dir)?;
        sessions.list_candidate_dispatches_for_day(&agent, &day_key)
    })
    .await
    {
        Ok(Ok(rows)) => Ok(rows),
        Ok(Err(err)) => Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "store_unavailable",
            &err.to_string(),
        )),
        Err(_) => Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "task_failed",
            "store task failed",
        )),
    }
}

pub async fn inbox(State(state): State<AppState>, _auth: OperatorAuth, uri: Uri) -> Response {
    let query = match axum::extract::Query::<InboxQuery>::try_from_uri(&uri) {
        Ok(query) => query.0,
        Err(err) => return error(StatusCode::BAD_REQUEST, "bad_query", &err.to_string()),
    };
    if query.limit == 0 || query.limit > 200 {
        return error(StatusCode::BAD_REQUEST, "bad_limit", "limit must be 1..200");
    }
    let (data_dir, mut agents) = {
        let config = state.config.read();
        let agents = match query.agent.as_deref() {
            Some(agent) if config.agent(agent).is_some() => vec![agent.to_string()],
            Some(_) => {
                return error(
                    StatusCode::NOT_FOUND,
                    "unknown_agent",
                    "unknown configured agent",
                );
            }
            None => config.agents.keys().cloned().collect::<Vec<_>>(),
        };
        (config.data_dir.clone(), agents)
    };
    agents.sort();

    let agents_for_digests = agents.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let soul = SoulProfileStore::shared(&data_dir)?;
        let user = UserModelStore::shared(&data_dir)?;
        let sessions = SqliteSessionBackend::new(&data_dir)?;
        let mut pending = Vec::new();
        let mut receipts = Vec::new();
        let mut dispatches_by_agent: HashMap<String, HashMap<String, Vec<CandidateDispatchRecord>>> =
            HashMap::new();
        for agent in agents {
            for proposal in soul.proposals(&agent, true)? {
                pending.push(json!({
                    "id": format!("soul:{agent}:{}", proposal.id), "kind": "soul_proposal", "agent": agent,
                    "created_at_unix": proposal.created_at_unix, "item": proposal,
                    "review_url": format!("/api/soul/proposals/{}/resolve", proposal.id),
                }));
            }
            for (id, receipt) in soul.reflections(&agent, 20)? {
                receipts.push(json!({"id": format!("reflection:{agent}:{id}"), "kind": "reflection_receipt", "agent": agent, "created_at_unix": receipt.ran_at_unix, "item": receipt}));
            }
            let mut by_day: HashMap<String, Vec<CandidateDispatchRecord>> = HashMap::new();
            for rec in sessions.list_candidate_dispatches_for_agent(&agent)? {
                by_day.entry(rec.day_key.clone()).or_default().push(rec);
            }
            dispatches_by_agent.insert(agent, by_day);
        }
        // The owner's User Model is shared; agent filtering only filters Soul
        // and reflection history, never hides global owner candidates.
        for candidate in user.list_pending_candidates()? {
            pending.push(json!({"id": format!("user_model:{}", candidate.id), "kind": "user_model_candidate", "created_at_unix": candidate.created_at_unix, "item": candidate, "review_url": format!("/api/user-model/candidates/{}/review", candidate.id)}));
        }
        Ok((pending, receipts, dispatches_by_agent))
    })
    .await;

    let (mut pending, mut receipts, mut dispatches_by_agent) = match result {
        Ok(Ok(triple)) => triple,
        Ok(Err(err)) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "store_unavailable",
                &err.to_string(),
            );
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "task_failed",
                "store task failed",
            );
        }
    };

    for agent in agents_for_digests {
        let digests = match state
            .mem
            .load_daily_digests(&agent, DAILY_DIGEST_INBOX_LIMIT)
            .await
        {
            Ok(d) => d,
            Err(err) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "store_unavailable",
                    &err.to_string(),
                );
            }
        };
        let mut agent_dispatches = dispatches_by_agent.remove(&agent).unwrap_or_default();
        for digest in digests {
            let created_at_unix = rfc3339_to_unix(&digest.created_at);
            let day_dispatches = agent_dispatches.remove(&digest.day_key).unwrap_or_default();
            let dispatch_by_cand: HashMap<&str, &CandidateDispatchRecord> = day_dispatches
                .iter()
                .map(|d| (d.candidate_id.as_str(), d))
                .collect();
            for item in digest.normalized_candidates() {
                let status = digest.candidate_status(&item);
                if status == ActionCandidateStatus::Pending {
                    let dispatch = dispatch_by_cand
                        .get(item.id.as_str())
                        .copied()
                        .map(dispatch_projection_json);
                    pending.push(json!({
                        "id": format!("sleep_action:{agent}:{}:{}", digest.day_key, item.id),
                        "kind": "sleep_action_candidate",
                        "agent": agent,
                        "created_at_unix": created_at_unix,
                        "item": {
                            "day_key": digest.day_key,
                            "agent_id": digest.agent_id,
                            "candidate_id": item.id,
                            "candidate": item.text,
                            "status": status,
                            "source_revision": digest.revision,
                            "dispatch": dispatch,
                        },
                        "review_url": format!("/api/review/action-candidates/{agent}/{}/resolve", digest.day_key),
                        "dispatch_url": format!("/api/review/action-candidates/{agent}/{}/dispatch", digest.day_key),
                    }));
                }
            }
            let digest_item = match project_digest_json(&digest, &day_dispatches) {
                Ok(v) => v,
                Err(err) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "serialization_failed",
                        &err.to_string(),
                    );
                }
            };
            receipts.push(json!({
                "id": format!("daily_digest:{agent}:{}", digest.day_key),
                "kind": "daily_digest",
                "agent": agent,
                "created_at_unix": created_at_unix,
                "item": digest_item,
                "edit_url": format!("/api/review/daily-digest/{agent}/{}", digest.day_key),
            }));
        }
    }

    pending.sort_by(|a, b| {
        (a["created_at_unix"].as_u64(), a["id"].as_str())
            .cmp(&(b["created_at_unix"].as_u64(), b["id"].as_str()))
    });
    receipts.sort_by(|a, b| {
        (b["created_at_unix"].as_u64(), b["id"].as_str())
            .cmp(&(a["created_at_unix"].as_u64(), a["id"].as_str()))
    });
    pending.extend(receipts);
    let total = pending.len();
    let items: Vec<_> = pending
        .into_iter()
        .skip(query.offset)
        .take(query.limit)
        .collect();
    let next = query.offset.saturating_add(items.len());
    (
        StatusCode::OK,
        axum::Json(json!({
            "items": items,
            "total": total,
            "next_offset": if next < total { Some(next) } else { None },
        })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateTextEdit {
    candidate_id: String,
    text: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDailyDigestRequest {
    narrative: String,
    #[serde(default)]
    settled_decisions: Option<Vec<String>>,
    #[serde(default)]
    why_and_tradeoffs: Option<Vec<String>>,
    #[serde(default)]
    latent_insights: Option<Vec<String>>,
    #[serde(default)]
    expected_revision: Option<u64>,
    #[serde(default)]
    candidate_edits: Option<Vec<CandidateTextEdit>>,
}

fn apply_candidate_edits(
    digest: &mut DailyDigest,
    edits: Vec<CandidateTextEdit>,
) -> Result<(), Box<Response>> {
    let mut seen = HashSet::with_capacity(edits.len());
    let mut normalized_edits = Vec::with_capacity(edits.len());
    for edit in edits {
        if let Err(err) = validate_candidate_id(&edit.candidate_id) {
            return Err(Box::new(error(
                StatusCode::BAD_REQUEST,
                "invalid",
                &err.to_string(),
            )));
        }
        let text = edit.text.trim();
        if text.is_empty() {
            return Err(Box::new(error(
                StatusCode::BAD_REQUEST,
                "invalid",
                "edited candidate text must not be empty",
            )));
        }
        if !seen.insert(edit.candidate_id.clone()) {
            return Err(Box::new(error(
                StatusCode::BAD_REQUEST,
                "invalid",
                "duplicate candidate_id in candidate_edits",
            )));
        }
        if !digest
            .candidate_items
            .iter()
            .any(|item| item.id == edit.candidate_id)
        {
            return Err(Box::new(error(
                StatusCode::NOT_FOUND,
                "action_candidate_not_found",
                "action candidate not found in daily digest",
            )));
        }
        normalized_edits.push((edit.candidate_id, text.to_string()));
    }

    // Materialize ID-keyed decisions before any display text changes so
    // editing candidate wording preserves review decisions and never mints new IDs.
    let prior_items = digest.candidate_items.clone();
    for item in &prior_items {
        if !digest.action_decisions.contains_key(&item.id) {
            let status = digest.candidate_status(item);
            if status != ActionCandidateStatus::Pending {
                digest.action_decisions.insert(item.id.clone(), status);
            }
        }
    }

    for (candidate_id, new_text) in normalized_edits {
        if let Some(item) = digest
            .candidate_items
            .iter_mut()
            .find(|item| item.id == candidate_id)
        {
            item.text = new_text;
        }
    }

    digest.action_candidates = digest
        .candidate_items
        .iter()
        .map(|item| item.text.clone())
        .collect();

    let mut text_counts: HashMap<String, usize> = HashMap::new();
    for item in &digest.candidate_items {
        *text_counts.entry(item.text.clone()).or_insert(0) += 1;
    }
    let id_set: HashSet<String> = digest
        .candidate_items
        .iter()
        .map(|item| item.id.clone())
        .collect();
    // Drop stale text keys before re-projecting ID-keyed decisions onto unique post-edit texts.
    digest
        .action_decisions
        .retain(|key, _| id_set.contains(key));
    for item in &digest.candidate_items {
        if text_counts.get(&item.text).copied() == Some(1)
            && let Some(status) = digest.action_decisions.get(&item.id).copied()
        {
            digest.action_decisions.insert(item.text.clone(), status);
        }
    }
    Ok(())
}

pub async fn update_daily_digest(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path((agent, day_key)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(resp) = validate_agent_and_day(&state, &agent, &day_key, false) {
        return *resp;
    }
    let req: UpdateDailyDigestRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string()),
    };
    let narrative = req.narrative.trim();
    if narrative.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "narrative must not be empty",
        );
    }

    let mut digest = match load_daily_digest(&state, &agent, &day_key).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    if let Some(expected) = req.expected_revision
        && expected != digest.revision
    {
        return revision_conflict(digest.revision);
    }

    digest.ensure_candidate_items();
    if let Some(edits) = req.candidate_edits
        && let Err(resp) = apply_candidate_edits(&mut digest, edits)
    {
        return *resp;
    }

    digest.narrative = narrative.to_string();
    if let Some(settled) = req.settled_decisions {
        digest.settled_decisions = clean_string_list(settled);
    }
    if let Some(why) = req.why_and_tradeoffs {
        digest.why_and_tradeoffs = clean_string_list(why);
    }
    if let Some(insights) = req.latent_insights {
        digest.latent_insights = clean_string_list(insights);
    }
    digest.owner_edited = true;
    let expected = digest.revision;
    let Some(next_revision) = expected.checked_add(1) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "daily digest revision overflow",
        );
    };
    digest.revision = next_revision;

    match state.mem.publish_daily_digest_cas(&digest, expected).await {
        Ok(DailyDigestCasResult::Updated(_)) => {}
        Ok(DailyDigestCasResult::RevisionConflict { current_revision }) => {
            return revision_conflict(current_revision);
        }
        Ok(DailyDigestCasResult::NotFound) => {
            return error(
                StatusCode::NOT_FOUND,
                "daily_digest_not_found",
                "daily digest not found",
            );
        }
        Err(err) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "store_unavailable",
                &err.to_string(),
            );
        }
    }

    let data_dir = state.config.read().data_dir.clone();
    let dispatches = match load_day_dispatches(data_dir, agent, day_key).await {
        Ok(rows) => rows,
        Err(resp) => return resp,
    };
    let digest_json = match project_digest_json(&digest, &dispatches) {
        Ok(v) => v,
        Err(err) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "serialization_failed",
                &err.to_string(),
            );
        }
    };
    (StatusCode::OK, axum::Json(json!({ "digest": digest_json }))).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveActionCandidateRequest {
    #[serde(default)]
    candidate_id: Option<String>,
    #[serde(default)]
    candidate: Option<String>,
    decision: ActionCandidateStatus,
    #[serde(default)]
    expected_revision: Option<u64>,
}

pub async fn resolve_action_candidate(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path((agent, day_key)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(resp) = validate_agent_and_day(&state, &agent, &day_key, false) {
        return *resp;
    }
    let req: ResolveActionCandidateRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string()),
    };
    if req.decision == ActionCandidateStatus::Pending {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "decision must be approved or dismissed",
        );
    }
    let requested_id = req.candidate_id.as_deref().map(str::trim);
    let requested_text = req.candidate.as_deref().map(str::trim);
    if requested_id.is_none_or(str::is_empty) && requested_text.is_none_or(str::is_empty) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "candidate_id or candidate must be provided",
        );
    }
    if let Some(id) = requested_id
        && let Err(err) = validate_candidate_id(id)
    {
        return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string());
    }

    let mut digest = match load_daily_digest(&state, &agent, &day_key).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    if let Some(expected) = req.expected_revision
        && expected != digest.revision
    {
        return revision_conflict(digest.revision);
    }
    digest.ensure_candidate_items();

    let item = if let Some(id) = requested_id {
        let Some(found) = digest.candidate_items.iter().find(|c| c.id == id).cloned() else {
            return error(
                StatusCode::NOT_FOUND,
                "action_candidate_not_found",
                "action candidate not found in daily digest",
            );
        };
        if let Some(text) = requested_text
            && !text.is_empty()
            && found.text.trim() != text
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid",
                "candidate text does not match candidate_id",
            );
        }
        found
    } else {
        let text = requested_text.unwrap_or_default();
        match digest.find_candidate_by_text(text) {
            Ok(Some(found)) => found,
            Ok(None) => {
                return error(
                    StatusCode::NOT_FOUND,
                    "action_candidate_not_found",
                    "action candidate not found in daily digest",
                );
            }
            Err(msg) => return error(StatusCode::CONFLICT, "ambiguous_candidate", msg),
        }
    };

    digest
        .action_decisions
        .insert(item.id.clone(), req.decision);
    let unique_text = digest
        .candidate_items
        .iter()
        .filter(|c| c.text == item.text)
        .count()
        == 1;
    if unique_text {
        digest
            .action_decisions
            .insert(item.text.clone(), req.decision);
    }

    let expected = digest.revision;
    let Some(next_revision) = expected.checked_add(1) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "daily digest revision overflow",
        );
    };
    digest.revision = next_revision;

    match state.mem.publish_daily_digest_cas(&digest, expected).await {
        Ok(DailyDigestCasResult::Updated(_)) => {}
        Ok(DailyDigestCasResult::RevisionConflict { current_revision }) => {
            return revision_conflict(current_revision);
        }
        Ok(DailyDigestCasResult::NotFound) => {
            return error(
                StatusCode::NOT_FOUND,
                "daily_digest_not_found",
                "daily digest not found",
            );
        }
        Err(err) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "store_unavailable",
                &err.to_string(),
            );
        }
    }

    (
        StatusCode::OK,
        axum::Json(json!({
            "digest": digest,
            "candidate_id": item.id,
            "candidate": item.text,
            "status": req.decision,
            "revision": digest.revision,
        })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchActionCandidateRequest {
    candidate_id: String,
    expected_revision: u64,
    request_id: String,
    harness: String,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    issue_ref: Option<String>,
    #[serde(default)]
    pr_ref: Option<String>,
    #[serde(default)]
    flow_id: Option<String>,
    #[serde(default)]
    context_brief: Option<String>,
}

fn tachi_estop_allows(config: &Config, tool: &str) -> bool {
    if !config.security.estop.enabled {
        return true;
    }
    let dir = config
        .config_path
        .parent()
        .unwrap_or_else(|| StdPath::new("."));
    let estop = reiclaw_policy::estop::EstopHandle::for_config(&config.security.estop, dir);
    let Ok(endpoint) = config.tachi.endpoint.trim().parse::<Uri>() else {
        return false;
    };
    estop.enforce_tool(tool).is_ok()
        && estop.enforce_tool("tachi_staff").is_ok()
        && estop.enforce_network(endpoint.host()).is_ok()
}

enum DispatchPolicyError {
    StorageRootChanged,
    Denied(String),
    Disabled,
    Unavailable(String),
    UnknownHarness(String),
}

fn resolve_live_dispatch(
    config: &Config,
    selected_data_dir: &StdPath,
    agent: &str,
    tracker: &PerSenderTracker,
    harness: &str,
) -> Result<(TachiConfig, PathBuf, String), DispatchPolicyError> {
    if config.data_dir != selected_data_dir {
        return Err(DispatchPolicyError::StorageRootChanged);
    }
    if !config.agents.get(agent).is_some_and(|entry| entry.enabled) {
        return Err(DispatchPolicyError::Denied(
            "delegation denied by current agent policy".to_string(),
        ));
    }
    if !tachi_estop_allows(config, "tachi_start") {
        return Err(DispatchPolicyError::Denied(
            "delegation blocked by emergency stop".to_string(),
        ));
    }
    if !config.tachi.enabled {
        return Err(DispatchPolicyError::Disabled);
    }
    let mut policy = SecurityPolicy::for_agent(config, agent)
        .map_err(|err| DispatchPolicyError::Denied(err.to_string()))?;
    policy
        .enforce_tool_operation(ToolOperation::Read, "tachi_start")
        .map_err(DispatchPolicyError::Denied)?;
    if !policy.is_tool_allowed("tachi_start") || !policy.can_act() {
        return Err(DispatchPolicyError::Denied(
            "delegation denied by current agent policy".to_string(),
        ));
    }
    policy.tracker = tracker.clone();
    if policy.is_rate_limited() {
        return Err(DispatchPolicyError::Denied(
            "Rate limit exceeded: action budget exhausted".to_string(),
        ));
    }
    let client = TachiStaffClient::from_config(&config.tachi, agent)
        .map_err(|err| DispatchPolicyError::Unavailable(err.to_string()))?;
    let profile = client
        .profile_for(harness)
        .map_err(|err| match err {
            TachiStaffError::UnknownHarness(msg) => DispatchPolicyError::UnknownHarness(msg),
            other => DispatchPolicyError::Unavailable(other.to_string()),
        })?
        .to_string();
    Ok((
        config.tachi.clone(),
        selected_data_dir.to_path_buf(),
        profile,
    ))
}

fn resolve_for_control(
    config: &Config,
    selected_data_dir: &StdPath,
    agent: &str,
    tracker: &PerSenderTracker,
    name: &str,
    operation: ToolOperation,
) -> Result<(TachiConfig, PathBuf), String> {
    if !tachi_estop_allows(config, name) {
        return Err("delegation blocked by emergency stop".to_string());
    }
    reiclaw_runtime::tools::delegation::resolve(
        config,
        selected_data_dir,
        agent,
        tracker,
        name,
        operation,
    )
}

fn map_policy_error(err: DispatchPolicyError) -> Response {
    match err {
        DispatchPolicyError::StorageRootChanged => error(
            StatusCode::CONFLICT,
            "storage_root_changed",
            "storage root changed while runtime ledger is active",
        ),
        DispatchPolicyError::Denied(msg) => error(StatusCode::FORBIDDEN, "denied", &msg),
        DispatchPolicyError::Disabled => error(
            StatusCode::BAD_REQUEST,
            "disabled",
            "tachi delegation is disabled",
        ),
        DispatchPolicyError::UnknownHarness(name) => error(
            StatusCode::BAD_REQUEST,
            "unknown_harness",
            &format!("unknown configured tachi harness '{name}'"),
        ),
        DispatchPolicyError::Unavailable(msg) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", &msg)
        }
    }
}

fn build_start_args(req: &DispatchActionCandidateRequest, raw_task: &str, harness: &str) -> Value {
    let mut start_args = json!({
        "request_id": req.request_id,
        "harness": harness,
        "task": raw_task,
        "staffing_reason": "explicit_user_request",
    });
    if let Some(obj) = start_args.as_object_mut() {
        for (key, value) in [
            ("issue_ref", req.issue_ref.as_deref()),
            ("pr_ref", req.pr_ref.as_deref()),
            ("flow_id", req.flow_id.as_deref()),
            ("context_brief", req.context_brief.as_deref()),
        ] {
            if let Some(v) = value {
                obj.insert(key.to_string(), Value::String(v.to_string()));
            }
        }
    }
    start_args
}

async fn dispatch_action_candidate(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Extension(context): Extension<Arc<DispatchContext>>,
    Path((agent, day_key)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(resp) = validate_agent_and_day(&state, &agent, &day_key, true) {
        return *resp;
    }
    let req: DispatchActionCandidateRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_arguments",
                &err.to_string(),
            );
        }
    };
    if let Err(err) = validate_candidate_id(&req.candidate_id) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_arguments",
            &err.to_string(),
        );
    }
    if req.expected_revision < 1 {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_arguments",
            "expected_revision must be >= 1",
        );
    }
    if !valid_request_id(&req.request_id) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_arguments",
            "invalid delegation request_id",
        );
    }
    let harness = req.harness.trim().to_string();
    if harness.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_arguments",
            "harness must not be empty",
        );
    }

    let digest = match load_daily_digest(&state, &agent, &day_key).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let Some(candidate_item) = digest
        .normalized_candidates()
        .into_iter()
        .find(|item| item.id == req.candidate_id)
    else {
        return error(
            StatusCode::NOT_FOUND,
            "action_candidate_not_found",
            "action candidate not found in daily digest",
        );
    };

    let raw_task = req
        .task
        .as_deref()
        .unwrap_or(&candidate_item.text)
        .trim()
        .to_string();
    if raw_task.is_empty() || raw_task.len() > MAX_TASK_BYTES {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_arguments",
            "candidate task must be 1..16384 bytes",
        );
    }
    for value in [raw_task.as_str(), req.request_id.as_str()]
        .into_iter()
        .chain(req.issue_ref.as_deref())
        .chain(req.pr_ref.as_deref())
        .chain(req.flow_id.as_deref())
    {
        if let Err(category) = reiclaw_tools::tachi_admission::scan_text(value) {
            return error(
                StatusCode::BAD_REQUEST,
                "forbidden_content",
                &category.to_string(),
            );
        }
    }
    let outbound_task =
        match prepare_outbound_task_with_brief(&raw_task, req.context_brief.as_deref()) {
            Ok(task) => task,
            Err(err @ (TaskBriefError::EmptyTask | TaskBriefError::BriefTooLarge { .. })) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_arguments",
                    &err.to_string(),
                );
            }
            Err(
                err @ (TaskBriefError::ProtectedIdentityMarker { .. }
                | TaskBriefError::Admission(_)),
            ) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "forbidden_content",
                    &err.to_string(),
                );
            }
        };

    let (tachi, data_dir, profile) = {
        let current = state.config.read();
        match resolve_live_dispatch(
            &current,
            &context.selected_data_dir,
            &agent,
            &context.tracker,
            &harness,
        ) {
            Ok(resolved) => resolved,
            Err(err) => return map_policy_error(err),
        }
    };
    let route_digest = route_fingerprint(&tachi, &agent);
    let payload_digest = sha256_json(&json!({
        "profile": profile,
        "task": raw_task,
        "outbound_task": outbound_task,
        "issue_ref": req.issue_ref,
        "pr_ref": req.pr_ref,
        "flow_id": req.flow_id,
        "context_brief": req.context_brief,
    }));

    let Ok(permit) = context.slots.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "control_busy",
            "candidate dispatch concurrency limit reached",
        );
    };

    let candidate_status = digest.candidate_status(&candidate_item);
    let allow_create = digest.revision == req.expected_revision
        && candidate_status != ActionCandidateStatus::Dismissed;
    let claim_input = CandidateDispatchClaimInput {
        agent_alias: agent.clone(),
        day_key: day_key.clone(),
        candidate_id: req.candidate_id.clone(),
        source_revision: req.expected_revision,
        request_id: req.request_id.clone(),
        harness: harness.clone(),
        route_digest: route_digest.clone(),
        payload_digest: payload_digest.clone(),
    };

    let db = match context
        .sessions_db
        .get_or_try_init(|| async {
            let db_dir = data_dir.clone();
            match tokio::task::spawn_blocking(move || {
                SqliteSessionBackend::new(&db_dir).map(Arc::new)
            })
            .await
            {
                Ok(Ok(db)) => Ok(db),
                Ok(Err(err)) => Err(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_error",
                    &err.to_string(),
                )),
                Err(_) => Err(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "task_failed",
                    "session store task failed",
                )),
            }
        })
        .await
    {
        Ok(db) => db.clone(),
        Err(resp) => return resp,
    };

    let claim = {
        let db = db.clone();
        let input = claim_input.clone();
        match tokio::task::spawn_blocking(move || db.claim_candidate_dispatch(&input, allow_create))
            .await
        {
            Ok(Ok(claim)) => claim,
            Ok(Err(err)) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_error",
                    &err.to_string(),
                );
            }
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "task_failed",
                    "claim task failed",
                );
            }
        }
    };

    match claim {
        CandidateDispatchClaim::Created(_) => {}
        CandidateDispatchClaim::Replay(existing) => {
            if existing.state == CandidateDispatchState::Bound {
                return (
                    StatusCode::OK,
                    axum::Json(json!({
                        "accepted": true,
                        "replayed": true,
                        "candidate_id": existing.candidate_id,
                        "source_revision": existing.source_revision,
                        "request_id": existing.request_id,
                        "dispatch_id": existing.dispatch_id,
                        "state": existing.state,
                    })),
                )
                    .into_response();
            }
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "code": "submission_unresolved",
                    "error": "candidate dispatch claim exists without confirmed receipt; automatic resubmission is blocked",
                    "claim": dispatch_projection_json(&existing),
                })),
            )
                .into_response();
        }
        CandidateDispatchClaim::AlreadyClaimed(existing) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "code": "candidate_already_dispatched",
                    "error": "candidate already has an active or completed dispatch claim",
                    "claim": dispatch_projection_json(&existing),
                })),
            )
                .into_response();
        }
        CandidateDispatchClaim::RequestConflict(_)
        | CandidateDispatchClaim::RequestIdInUse(_)
        | CandidateDispatchClaim::RequestConflictExternal => {
            return error(
                StatusCode::CONFLICT,
                "request_conflict",
                "request_id conflicts with an existing delegation claim",
            );
        }
        CandidateDispatchClaim::NotAllowedToCreate => {
            if digest.revision != req.expected_revision {
                return revision_conflict(digest.revision);
            }
            return error(
                StatusCode::CONFLICT,
                "candidate_dismissed",
                "dismissed action candidate cannot be dispatched",
            );
        }
    }

    let start_args = build_start_args(&req, &raw_task, &harness);
    let (live, tracker, selected_data_dir, agent_clone) = (
        state.config.clone(),
        context.tracker.clone(),
        context.selected_data_dir.clone(),
        agent.clone(),
    );
    let service = TachiDelegation::new(&agent, move |name, operation| {
        let current = live.read();
        resolve_for_control(
            &current,
            &selected_data_dir,
            &agent_clone,
            &tracker,
            name,
            operation,
        )
    });

    let mut shutdown = state.shutdown_tx.subscribe();
    let control_timeout = context.timeout;
    let finalizer = DispatchFinalizer {
        db: db.clone(),
        agent: agent.clone(),
        day_key: day_key.clone(),
        candidate_id: req.candidate_id.clone(),
        request_id: req.request_id.clone(),
        payload_digest,
        route_digest,
    };
    let agent_spawn = agent.clone();
    let submitted = reiclaw_log::spawn!(async move {
        let _permit = permit;
        let outcome = reiclaw_api::TOOL_LOOP_THREAD_ID
            .scope(Some(format!("review_dispatch:{agent_spawn}")), async {
                tokio::select! {
                    result = tokio::time::timeout(
                        control_timeout,
                        service.control(DelegationOperation::Start, start_args),
                    ) => result.ok(),
                    _ = async {
                        while !*shutdown.borrow() {
                            if shutdown.changed().await.is_err() {
                                break;
                            }
                        }
                    } => None,
                }
            })
            .await;

        finalizer.finish(outcome).await
    });

    match submitted.await {
        Ok(resp) => resp,
        Err(_) => {
            let _ = tokio::task::spawn_blocking(move || {
                db.mark_candidate_dispatch_unknown(
                    &agent,
                    &day_key,
                    &req.candidate_id,
                    &req.request_id,
                )
            })
            .await;
            error(
                StatusCode::GATEWAY_TIMEOUT,
                "submission_unknown",
                "candidate dispatch outcome could not be confirmed",
            )
        }
    }
}

struct DispatchFinalizer {
    db: Arc<SqliteSessionBackend>,
    agent: String,
    day_key: String,
    candidate_id: String,
    request_id: String,
    payload_digest: String,
    route_digest: String,
}

impl DispatchFinalizer {
    async fn mark_unknown(&self) {
        let (db, a, d, c, r) = (
            self.db.clone(),
            self.agent.clone(),
            self.day_key.clone(),
            self.candidate_id.clone(),
            self.request_id.clone(),
        );
        let _ =
            tokio::task::spawn_blocking(move || db.mark_candidate_dispatch_unknown(&a, &d, &c, &r))
                .await;
    }

    async fn finish(
        self,
        outcome: Option<anyhow::Result<reiclaw_api::tool::ToolResult>>,
    ) -> Response {
        let Some(Ok(tool)) = outcome else {
            self.mark_unknown().await;
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "submission_unknown",
                "candidate dispatch timed out or shut down before receiving receipt",
            );
        };
        let Some(output) = tool
            .output
            .data()
            .filter(|value| value.is_object())
            .cloned()
        else {
            self.mark_unknown().await;
            return error(
                StatusCode::BAD_GATEWAY,
                "invalid_receipt",
                "invalid delegation receipt from Tachi",
            );
        };

        if tool.success {
            let Some(dispatch_id) = output["receipt"]["dispatch_id"]
                .as_str()
                .or_else(|| output["dispatch_id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                self.mark_unknown().await;
                return error(
                    StatusCode::BAD_GATEWAY,
                    "invalid_receipt",
                    "Tachi receipt omitted dispatch_id",
                );
            };
            let (db, a, d, c, r, disp) = (
                self.db.clone(),
                self.agent.clone(),
                self.day_key.clone(),
                self.candidate_id.clone(),
                self.request_id.clone(),
                dispatch_id.clone(),
            );
            return match tokio::task::spawn_blocking(move || {
                db.bind_candidate_dispatch(&a, &d, &c, &r, &disp)
            })
            .await
            {
                Ok(Ok(record)) => (
                    StatusCode::OK,
                    axum::Json(json!({
                        "accepted": true,
                        "replayed": false,
                        "candidate_id": record.candidate_id,
                        "source_revision": record.source_revision,
                        "request_id": record.request_id,
                        "dispatch_id": record.dispatch_id,
                        "state": record.state,
                        "receipt": output.get("receipt").cloned().unwrap_or_else(|| json!({"dispatch_id": dispatch_id})),
                    })),
                )
                    .into_response(),
                _ => error(
                    StatusCode::BAD_GATEWAY,
                    "binding_write_failed",
                    "Tachi started run, but binding local candidate claim failed",
                ),
            };
        }

        let code = output["code"].as_str().unwrap_or("unknown");
        let detail = tool
            .error
            .unwrap_or_else(|| "delegation start failed".to_string());
        if code == "binding_write_failed"
            && let Some(dispatch_id) = output["receipt"]["dispatch_id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        {
            let (db, a, d, c, r) = (
                self.db.clone(),
                self.agent.clone(),
                self.day_key.clone(),
                self.candidate_id.clone(),
                self.request_id.clone(),
            );
            let bound = tokio::task::spawn_blocking(move || {
                db.bind_candidate_dispatch(&a, &d, &c, &r, &dispatch_id)
            })
            .await;
            if !matches!(bound, Ok(Ok(_))) {
                self.mark_unknown().await;
            }
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(json!({
                    "code": "binding_write_failed",
                    "error": detail,
                    "receipt": output.get("receipt"),
                })),
            )
                .into_response();
        }

        let provably_unsent = matches!(
            code,
            "denied"
                | "approval_required"
                | "storage_root_changed"
                | "disabled"
                | "invalid_config"
                | "invalid_arguments"
                | "unknown_harness"
                | "forbidden_content"
                | "source_unavailable"
                | "unavailable"
        );
        if provably_unsent {
            let (db, a, d, c, r, p, rt) = (
                self.db.clone(),
                self.agent.clone(),
                self.day_key.clone(),
                self.candidate_id.clone(),
                self.request_id.clone(),
                self.payload_digest.clone(),
                self.route_digest.clone(),
            );
            let released = tokio::task::spawn_blocking(move || {
                db.release_unsent_candidate_dispatch(&a, &d, &c, &r, &p, &rt)
            })
            .await;
            if matches!(released, Ok(Ok(true))) {
                let status = match code {
                    "denied" | "approval_required" => StatusCode::FORBIDDEN,
                    "storage_root_changed" => StatusCode::CONFLICT,
                    "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
                    _ => StatusCode::BAD_REQUEST,
                };
                return error(status, code, &detail);
            }
        }

        self.mark_unknown().await;
        let public_code = if provably_unsent {
            "submission_unknown"
        } else {
            code
        };
        let status = match public_code {
            "submission_unknown" => StatusCode::GATEWAY_TIMEOUT,
            "storage_error" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        };
        error(status, public_code, &detail)
    }
}

#[cfg(test)]
mod tests;
