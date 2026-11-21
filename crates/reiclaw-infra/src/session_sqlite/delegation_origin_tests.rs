use super::{
    DispatchBindingRow, ProxyContinuationAdmission, ProxyContinuationState, SqliteSessionBackend,
};
use crate::session_backend::SessionBackend;
use reiclaw_api::delegation_request::{
    DelegationOrigin, DelegationOriginScope, DelegationRequestClaim,
};
use reiclaw_api::model_provider::ChatMessage;
use rusqlite::{Connection, params};
use std::sync::{Arc, Barrier};

const AGENT: &str = "owner-agent";
const ROUTE: &str = "origin-route";
const DIGEST: &str = "task-digest";

fn origin(session_key: &str) -> DelegationOrigin {
    DelegationOrigin {
        session_key: session_key.into(),
        scope: DelegationOriginScope::SessionFacts {
            operator_subject: "a".repeat(64),
            agent_id: "stable-owner-agent".into(),
            memory_session_id: Some("canonical-memory-session".into()),
        },
    }
}

fn seed_session(store: &SqliteSessionBackend, key: &str, agent: &str, content: &str) {
    store.set_session_agent_alias(key, agent).unwrap();
    store.append(key, &ChatMessage::user(content)).unwrap();
}

fn binding(store: &SqliteSessionBackend, request: &str) -> DispatchBindingRow {
    store
        .list_dispatch_bindings_page(0, 100)
        .unwrap()
        .into_iter()
        .find(|row| row.agent_alias == AGENT && row.request_id == request)
        .unwrap()
}

fn bind(
    store: &SqliteSessionBackend,
    request: &str,
    dispatch: &str,
    source: Option<&DelegationOrigin>,
) -> DispatchBindingRow {
    assert_eq!(
        store
            .claim_delegation_request_with_origin(AGENT, request, DIGEST, ROUTE, source)
            .unwrap(),
        DelegationRequestClaim::Created
    );
    store
        .bind_delegation_request(AGENT, request, DIGEST, ROUTE, dispatch)
        .unwrap();
    binding(store, request)
}

#[test]
fn origin_is_persisted_with_initial_claim_and_never_replaced_by_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(&store, "source-a", AGENT, "original source");
    seed_session(&store, "source-b", AGENT, "different source");
    let source = origin("source-a");
    assert_eq!(
        store
            .claim_delegation_request_with_origin(AGENT, "request", DIGEST, ROUTE, Some(&source))
            .unwrap(),
        DelegationRequestClaim::Created
    );
    let claimed = store
        .read_delegation_request(AGENT, "request")
        .unwrap()
        .unwrap();
    assert_eq!(claimed.origin.as_ref(), Some(&source));
    assert!(
        claimed.dispatch_id.is_none(),
        "origin precedes external acceptance"
    );
    assert_eq!(
        store
            .claim_delegation_request_with_origin(AGENT, "request", DIGEST, ROUTE, Some(&source))
            .unwrap(),
        DelegationRequestClaim::Existing { dispatch_id: None }
    );
    for replacement in [Some(origin("source-b")), None] {
        assert_eq!(
            store
                .claim_delegation_request_with_origin(
                    AGENT,
                    "request",
                    DIGEST,
                    ROUTE,
                    replacement.as_ref(),
                )
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
    }
    store
        .bind_delegation_request(AGENT, "request", DIGEST, ROUTE, "dispatch")
        .unwrap();
    drop(store);
    let reopened = SqliteSessionBackend::new(dir.path()).unwrap();
    let persisted = reopened
        .read_delegation_request(AGENT, "request")
        .unwrap()
        .unwrap();
    assert_eq!(persisted.origin, Some(source.clone()));
    assert_eq!(persisted.route_digest.as_deref(), Some(ROUTE));
    assert_eq!(
        reopened
            .claim_delegation_request_with_origin(AGENT, "request", DIGEST, ROUTE, Some(&source))
            .unwrap(),
        DelegationRequestClaim::Existing {
            dispatch_id: Some("dispatch".into())
        }
    );
}

#[test]
fn origin_admission_rejects_missing_foreign_and_invalid_authority_without_claiming() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(&store, "foreign", "different-agent", "foreign input");
    seed_session(&store, "owned", AGENT, "owner input");
    let invalid_subject = DelegationOrigin {
        session_key: "owned".into(),
        scope: DelegationOriginScope::SessionFacts {
            operator_subject: "not-a-credential-hash".into(),
            agent_id: "stable-owner-agent".into(),
            memory_session_id: Some("canonical-memory-session".into()),
        },
    };
    for source in [origin("missing"), origin("foreign"), invalid_subject] {
        assert!(
            store
                .claim_delegation_request_with_origin(
                    AGENT,
                    "request",
                    DIGEST,
                    ROUTE,
                    Some(&source)
                )
                .is_err()
        );
        assert!(
            store
                .read_delegation_request(AGENT, "request")
                .unwrap()
                .is_none()
        );
    }
    store
        .conn
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_origin_claim AFTER INSERT ON session_delegations
         BEGIN SELECT RAISE(ABORT, 'fixture admission failure'); END;",
        )
        .unwrap();
    let error = store
        .claim_delegation_request_with_origin(
            AGENT,
            "request",
            DIGEST,
            ROUTE,
            Some(&origin("owned")),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("fixture admission failure"));
    assert!(
        store
            .read_delegation_request(AGENT, "request")
            .unwrap()
            .is_none()
    );
}

#[test]
fn concurrent_different_sources_cannot_replace_the_winning_claim() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(&store, "source-a", AGENT, "source a");
    seed_session(&store, "source-b", AGENT, "source b");
    let handles = [
        SqliteSessionBackend::new(dir.path()).unwrap(),
        SqliteSessionBackend::new(dir.path()).unwrap(),
    ];
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = handles
        .into_iter()
        .zip(["source-a", "source-b"])
        .map(|(handle, key)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let source = origin(key);
                barrier.wait();
                let claim = handle
                    .claim_delegation_request_with_origin(
                        AGENT,
                        "request",
                        DIGEST,
                        ROUTE,
                        Some(&source),
                    )
                    .unwrap();
                (source, claim)
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|(_, claim)| *claim == DelegationRequestClaim::Created)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, claim)| *claim == DelegationRequestClaim::Conflict)
            .count(),
        1
    );
    let winner = results
        .into_iter()
        .find(|(_, claim)| *claim == DelegationRequestClaim::Created)
        .unwrap()
        .0;
    let persisted = store
        .read_delegation_request(AGENT, "request")
        .unwrap()
        .unwrap();
    assert_eq!(persisted.origin, Some(winner));
    assert!(persisted.dispatch_id.is_none());
}

#[test]
fn session_facts_read_only_the_exact_source_and_current_checkpoint_format() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(&store, "source-a", AGENT, "needle: permitted source");
    seed_session(
        &store,
        "source-b",
        AGENT,
        "needle: unrelated same-agent secret",
    );
    seed_session(
        &store,
        "source-c",
        "different-agent",
        "needle: foreign secret",
    );
    for key in ["source-a", "source-b", "source-c"] {
        let watermark: i64 = store
            .conn
            .lock()
            .query_row(
                "SELECT MAX(id) FROM sessions WHERE session_key = ?1",
                [key],
                |row| row.get(0),
            )
            .unwrap();
        store
            .stage_checkpoint(key, watermark, &format!("checkpoint for {key}"))
            .unwrap();
    }
    store.conn.lock().execute(
        "INSERT INTO session_checkpoints
         (session_key,slot,up_to_message_id,digest,created_at,updated_at,format_version)
         SELECT session_key,'active',up_to_message_id,'unverified legacy checkpoint',created_at,updated_at,0
         FROM session_checkpoints WHERE session_key = 'source-a' AND slot = 'staged'", [],
    ).unwrap();
    let row = bind(&store, "request", "dispatch", Some(&origin("source-a")));
    let facts = store
        .delegation_session_facts(&row, "needle")
        .unwrap()
        .unwrap();
    assert_eq!(facts.checkpoint_digests, ["checkpoint for source-a"]);
    assert_eq!(facts.transcript_hits.len(), 1);
    assert_eq!(facts.transcript_hits[0].session_key, "source-a");
    assert_eq!(facts.transcript_hits[0].content, "needle: permitted source");

    let mut replaced = row.clone();
    replaced.origin = Some(origin("source-b"));
    assert!(!store.delegation_origin_is_current(&replaced).unwrap());
    assert!(
        store
            .delegation_session_facts(&replaced, "needle")
            .unwrap()
            .is_none()
    );
    replaced = row.clone();
    replaced.request_id = "different-request".into();
    assert!(!store.delegation_origin_is_current(&replaced).unwrap());
    replaced = row;
    replaced.route_digest = Some("different-route".into());
    assert!(!store.delegation_origin_is_current(&replaced).unwrap());
}

#[test]
fn source_only_and_legacy_claims_never_inherit_another_requests_fact_grant() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(&store, "source", AGENT, "needle: authorized facts");
    let granted = bind(
        &store,
        "granted",
        "shared-dispatch",
        Some(&origin("source")),
    );
    let source_only = DelegationOrigin {
        session_key: "source".into(),
        scope: DelegationOriginScope::SourceOnly,
    };
    let rows = [
        bind(&store, "source-only", "shared-dispatch", Some(&source_only)),
        bind(&store, "legacy", "shared-dispatch", None),
    ];
    assert!(store.delegation_origin_is_current(&granted).unwrap());
    for row in rows {
        assert!(!store.delegation_origin_is_current(&row).unwrap());
        assert!(
            store
                .delegation_session_facts(&row, "needle")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.reserve_proxy_continuation(&row, 1, 3).unwrap(),
            ProxyContinuationAdmission::SourceUnavailable
        );
        assert_eq!(
            store
                .claim_delegation_request_with_origin(
                    AGENT,
                    &row.request_id,
                    DIGEST,
                    ROUTE,
                    Some(&origin("source")),
                )
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
    }
    assert_eq!(binding(&store, "granted").proxy_continuations, 0);
}

#[test]
fn session_lifecycle_revocation_survives_key_reuse_and_preserves_unknown_history() {
    for action in ["delete", "clear", "cleanup", "clear-agent", "rename-agent"] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionBackend::new(dir.path()).unwrap();
        seed_session(&store, "source", AGENT, "needle: original");
        let source = origin("source");
        let row = bind(&store, "request", "dispatch", Some(&source));
        let ProxyContinuationAdmission::Reserved(reservation) =
            store.reserve_proxy_continuation(&row, 7, 3).unwrap()
        else {
            panic!("valid source did not reserve");
        };
        assert!(store.mark_proxy_continuation_unknown(&reservation).unwrap());
        let before = binding(&store, "request");
        let before_nonce: String = store.conn.lock().query_row(
            "SELECT proxy_continuation_state FROM session_delegations WHERE request_id = 'request'",
            [], |row| row.get(0),
        ).unwrap();
        match action {
            "delete" => {
                assert!(store.delete_session("source").unwrap());
            }
            "clear" => {
                assert_eq!(store.clear_messages("source").unwrap(), 1);
            }
            "cleanup" => {
                store.conn.lock().execute(
                    "UPDATE session_metadata SET last_activity = '2000-01-01T00:00:00Z' WHERE session_key = 'source'", [],
                ).unwrap();
                assert_eq!(store.cleanup_stale(1).unwrap(), 1);
            }
            "clear-agent" => {
                assert_eq!(store.clear_agent_attribution(AGENT).unwrap(), 1);
            }
            "rename-agent" => {
                assert_eq!(
                    store
                        .rename_agent_attribution(AGENT, "renamed-agent")
                        .unwrap(),
                    1
                );
            }
            _ => unreachable!(),
        }
        seed_session(&store, "source", AGENT, "needle: replacement session");
        drop(store);
        let reopened = SqliteSessionBackend::new(dir.path()).unwrap();
        let after = binding(&reopened, "request");
        assert_eq!(
            after, before,
            "{action} must retain task and attempt history"
        );
        assert_eq!(
            after.proxy_continuation_state,
            ProxyContinuationState::Unknown
        );
        assert_eq!(after.proxy_continuations, 1);
        assert_eq!(after.last_proxy_revision, Some(7));
        let after_nonce: String = reopened.conn.lock().query_row(
            "SELECT proxy_continuation_state FROM session_delegations WHERE request_id = 'request'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(after_nonce, before_nonce);
        assert!(
            !reopened.delegation_origin_is_current(&row).unwrap(),
            "{action}"
        );
        assert!(
            reopened
                .delegation_session_facts(&row, "needle")
                .unwrap()
                .is_none(),
            "{action}"
        );
        assert!(
            !matches!(
                reopened.reserve_proxy_continuation(&row, 8, 3).unwrap(),
                ProxyContinuationAdmission::Reserved(_)
            ),
            "{action} must never authorize another attempt"
        );
        assert_eq!(
            reopened
                .claim_delegation_request_with_origin(
                    AGENT,
                    "request",
                    DIGEST,
                    ROUTE,
                    Some(&source),
                )
                .unwrap(),
            DelegationRequestClaim::Existing {
                dispatch_id: Some("dispatch".into())
            }
        );
        assert!(
            !reopened.delegation_origin_is_current(&row).unwrap(),
            "replay after {action}"
        );
    }
}

#[test]
fn failed_session_clear_rolls_back_revocation_with_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(&store, "source", AGENT, "needle: still authorized");
    let row = bind(&store, "request", "dispatch", Some(&origin("source")));
    store
        .conn
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_session_delete BEFORE DELETE ON sessions
         BEGIN SELECT RAISE(ABORT, 'fixture deletion failure'); END;",
        )
        .unwrap();
    assert!(store.clear_messages("source").is_err());
    assert!(store.delegation_origin_is_current(&row).unwrap());
    let facts = store
        .delegation_session_facts(&row, "needle")
        .unwrap()
        .unwrap();
    assert_eq!(facts.transcript_hits.len(), 1);
    assert_eq!(facts.transcript_hits[0].content, "needle: still authorized");
}

#[test]
fn shared_budget_owner_does_not_replace_the_request_source_authority() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(&store, "source-a", AGENT, "needle: source a");
    seed_session(&store, "source-b", AGENT, "needle: source b");
    let a = bind(
        &store,
        "request-a",
        "shared-dispatch",
        Some(&origin("source-a")),
    );
    let b = bind(
        &store,
        "request-b",
        "shared-dispatch",
        Some(&origin("source-b")),
    );
    let ProxyContinuationAdmission::Reserved(first) =
        store.reserve_proxy_continuation(&a, 3, 3).unwrap()
    else {
        panic!("source a did not reserve");
    };
    assert!(store.settle_proxy_continuation(&first).unwrap());
    store.clear_messages("source-a").unwrap();
    assert!(!store.delegation_origin_is_current(&a).unwrap());
    assert!(store.delegation_origin_is_current(&b).unwrap());
    let facts = store
        .delegation_session_facts(&b, "needle")
        .unwrap()
        .unwrap();
    assert_eq!(facts.transcript_hits.len(), 1);
    assert_eq!(facts.transcript_hits[0].content, "needle: source b");
    let ProxyContinuationAdmission::Reserved(second) =
        store.reserve_proxy_continuation(&b, 4, 3).unwrap()
    else {
        panic!("source b's independent authority was lost with source a");
    };
    assert_eq!(
        second.round(),
        2,
        "both requests consume one dispatch budget"
    );
    assert_eq!(binding(&store, "request-a").proxy_continuations, 2);
    assert_eq!(binding(&store, "request-b").proxy_continuations, 0);
    assert!(!store.delegation_origin_is_current(&a).unwrap());
    assert!(
        store
            .delegation_session_facts(&a, "needle")
            .unwrap()
            .is_none()
    );
}

#[test]
fn migration_never_backfills_origin_or_erases_preexisting_attempts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
    let old = Connection::open(dir.path().join("sessions/sessions.db")).unwrap();
    old.execute_batch(
        "CREATE TABLE session_delegations (
            agent_alias TEXT NOT NULL, request_id TEXT NOT NULL,
            request_digest TEXT NOT NULL, route_digest TEXT, dispatch_id TEXT,
            proxy_continuations INTEGER NOT NULL DEFAULT 0, last_proxy_revision INTEGER,
            PRIMARY KEY (agent_alias, request_id)
         );",
    )
    .unwrap();
    old.execute(
        "INSERT INTO session_delegations VALUES (?1, 'legacy', ?2, ?3, 'dispatch', 2, 17)",
        params![AGENT, DIGEST, ROUTE],
    )
    .unwrap();
    drop(old);
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    seed_session(
        &store,
        "source",
        AGENT,
        "needle: unrelated existing history",
    );
    let row = binding(&store, "legacy");
    assert!(row.origin.is_none());
    assert_eq!(row.proxy_continuations, 2);
    assert_eq!(row.last_proxy_revision, Some(17));
    assert_eq!(
        row.proxy_continuation_state,
        ProxyContinuationState::Unknown
    );
    assert!(!store.delegation_origin_is_current(&row).unwrap());
    assert!(
        store
            .delegation_session_facts(&row, "needle")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .claim_delegation_request_with_origin(
                AGENT,
                "legacy",
                DIGEST,
                ROUTE,
                Some(&origin("source")),
            )
            .unwrap(),
        DelegationRequestClaim::Conflict
    );
    drop(store);
    let reopened = SqliteSessionBackend::new(dir.path()).unwrap();
    assert_eq!(binding(&reopened, "legacy"), row);
}
