//! Unified owner review view. Soul and User Model stores remain canonical;
//! review actions use their existing operator-gated endpoints.
//! Collaborative DailyDigest edits and sleep action-candidate confirmations
//! persist to `memorycore.db` via the gateway's `Memory` handle.

use crate::AppState;
use crate::operator_auth::OperatorAuth;
use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use reiclaw_api::memory_traits::{ActionCandidateStatus, validate_agent_id, validate_non_soul_key};
use reiclaw_memory::companion::{SoulProfileStore, UserModelStore};

const DAILY_DIGEST_INBOX_LIMIT: usize = 14;
const DAILY_DIGEST_LOOKUP_LIMIT: usize = 60;

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
    (
        status,
        axum::Json(serde_json::json!({"code": code, "error": message})),
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
        let mut pending = Vec::new();
        let mut receipts = Vec::new();
        for agent in agents {
            for proposal in soul.proposals(&agent, true)? {
                pending.push(serde_json::json!({
                    "id": format!("soul:{agent}:{}", proposal.id), "kind": "soul_proposal", "agent": agent,
                    "created_at_unix": proposal.created_at_unix, "item": proposal,
                    "review_url": format!("/api/soul/proposals/{}/resolve", proposal.id),
                }));
            }
            for (id, receipt) in soul.reflections(&agent, 20)? {
                receipts.push(serde_json::json!({"id": format!("reflection:{agent}:{id}"), "kind": "reflection_receipt", "agent": agent, "created_at_unix": receipt.ran_at_unix, "item": receipt}));
            }
        }
        // The owner's User Model is shared; agent filtering only filters Soul
        // and reflection history, never hides global owner candidates.
        for candidate in user.list_pending_candidates()? {
            pending.push(serde_json::json!({"id": format!("user_model:{}", candidate.id), "kind": "user_model_candidate", "created_at_unix": candidate.created_at_unix, "item": candidate, "review_url": format!("/api/user-model/candidates/{}/review", candidate.id)}));
        }
        Ok((pending, receipts))
    })
    .await;

    let (mut pending, mut receipts) = match result {
        Ok(Ok(pair)) => pair,
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
        for digest in digests {
            let created_at_unix = rfc3339_to_unix(&digest.created_at);
            for (idx, candidate) in digest.action_candidates.iter().enumerate() {
                let status = digest
                    .action_decisions
                    .get(candidate)
                    .copied()
                    .unwrap_or_default();
                if status == ActionCandidateStatus::Pending {
                    pending.push(serde_json::json!({
                        "id": format!("sleep_action:{agent}:{}:{idx}", digest.day_key),
                        "kind": "sleep_action_candidate",
                        "agent": agent,
                        "created_at_unix": created_at_unix,
                        "item": {
                            "day_key": digest.day_key,
                            "agent_id": digest.agent_id,
                            "candidate": candidate,
                            "status": status,
                        },
                        "review_url": format!("/api/review/action-candidates/{agent}/{}/resolve", digest.day_key),
                    }));
                }
            }
            receipts.push(serde_json::json!({
                "id": format!("daily_digest:{agent}:{}", digest.day_key),
                "kind": "daily_digest",
                "agent": agent,
                "created_at_unix": created_at_unix,
                "item": digest,
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
        axum::Json(serde_json::json!({
            "items": items,
            "total": total,
            "next_offset": if next < total { Some(next) } else { None },
        })),
    )
        .into_response()
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
}

pub async fn update_daily_digest(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path((agent, day_key)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if state.config.read().agent(&agent).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "unknown_agent",
            "unknown configured agent",
        );
    }
    if let Err(err) = validate_agent_id(&agent) {
        return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string());
    }
    if let Err(err) = validate_non_soul_key(&day_key, "day_key") {
        return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string());
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

    let digests = match state
        .mem
        .load_daily_digests(&agent, DAILY_DIGEST_LOOKUP_LIMIT)
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
    let Some(mut digest) = digests.into_iter().find(|d| d.day_key == day_key) else {
        return error(
            StatusCode::NOT_FOUND,
            "daily_digest_not_found",
            "daily digest not found",
        );
    };

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

    if let Err(err) = state.mem.publish_daily_digest(&digest).await {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "store_unavailable",
            &err.to_string(),
        );
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "digest": digest })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveActionCandidateRequest {
    candidate: String,
    decision: ActionCandidateStatus,
}

pub async fn resolve_action_candidate(
    State(state): State<AppState>,
    _auth: OperatorAuth,
    Path((agent, day_key)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if state.config.read().agent(&agent).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "unknown_agent",
            "unknown configured agent",
        );
    }
    if let Err(err) = validate_agent_id(&agent) {
        return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string());
    }
    if let Err(err) = validate_non_soul_key(&day_key, "day_key") {
        return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string());
    }
    let req: ResolveActionCandidateRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => return error(StatusCode::BAD_REQUEST, "invalid", &err.to_string()),
    };
    let candidate = req.candidate.trim();
    if candidate.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "candidate must not be empty",
        );
    }
    if req.decision == ActionCandidateStatus::Pending {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "decision must be approved or dismissed",
        );
    }

    let digests = match state
        .mem
        .load_daily_digests(&agent, DAILY_DIGEST_LOOKUP_LIMIT)
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
    let Some(mut digest) = digests.into_iter().find(|d| d.day_key == day_key) else {
        return error(
            StatusCode::NOT_FOUND,
            "daily_digest_not_found",
            "daily digest not found",
        );
    };

    let Some(canonical_candidate) = digest
        .action_candidates
        .iter()
        .find(|c| c.as_str() == candidate)
        .cloned()
    else {
        return error(
            StatusCode::NOT_FOUND,
            "action_candidate_not_found",
            "action candidate not found in daily digest",
        );
    };

    digest
        .action_decisions
        .insert(canonical_candidate.clone(), req.decision);

    if let Err(err) = state.mem.publish_daily_digest(&digest).await {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "store_unavailable",
            &err.to_string(),
        );
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "digest": digest,
            "candidate": canonical_candidate,
            "status": req.decision,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
