use super::*;
use futures_util::FutureExt;
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
use zeroclaw_tools::tachi_delegation::route_fingerprint;

fn fixture_config() -> (tempfile::TempDir, Config) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: tmp.path().to_path_buf(),
        ..Config::default()
    };
    config.tachi.enabled = true;
    config.tachi.result_notification = Some(zeroclaw_config::tachi::DelegationNotificationConfig {
        bridge: "tg".into(),
        recipient: "owner".into(),
        thread_id: None,
    });
    config.gateway.bridges.insert(
        "tg".into(),
        zeroclaw_config::schema::GatewayBridgeConfig {
            token_hash: "ab".repeat(32),
            ..Default::default()
        },
    );
    config.agents.insert("butler".into(), Default::default());
    (tmp, config)
}

/// Seed a binding under the CURRENT route so the observer's provenance
/// check passes (pass `None` to seed a stale route for mismatch tests).
fn seed_binding(
    backend: &SqliteSessionBackend,
    config: &Config,
    agent: &str,
    request: &str,
    dispatch: &str,
    route: Option<&str>,
) {
    let digest = route
        .unwrap_or(&route_fingerprint(&config.tachi, agent))
        .to_string();
    backend
        .claim_delegation_request(agent, request, "request-digest", &digest)
        .expect("claim");
    backend
        .bind_delegation_request(agent, request, "request-digest", &digest, dispatch)
        .expect("bind");
}

fn status(state: RunState) -> RunStatus {
    RunStatus {
        dispatch_id: "d".into(),
        state,
        revision: None,
        closure_kind: None,
        result_written: None,
        updated_at: None,
        read_projection: None,
    }
}

fn delegation_rows(tmp: &tempfile::TempDir) -> Vec<serde_json::Value> {
    BridgeOutbox::shared(tmp.path())
        .unwrap()
        .list("tg", 0, 100)
        .unwrap()
        .into_iter()
        .filter(|item| item["source_kind"] == "delegation")
        .collect()
}

fn completed_fetch(
    _agent: &str,
    _dispatch: &str,
    _route: &zeroclaw_config::tachi::TachiConfig,
) -> BoxFuture<'static, anyhow::Result<RunStatus>> {
    async { Ok(status(RunState::Completed)) }.boxed()
}

#[tokio::test]
async fn terminal_states_notify_once_agents_are_scoped_and_unknown_never_notifies() {
    let (tmp, mut config) = fixture_config();
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    // Two agents sharing the SAME request id must produce two distinct
    // notifications (agent-scoped identity).
    config.agents.insert("maid".into(), Default::default());
    let shared = RwLock::new(config);
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-00000001",
        "disp-1",
        None,
    );
    seed_binding(
        &backend,
        &shared.read(),
        "maid",
        "req-00000001",
        "disp-2",
        None,
    );
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-00000002",
        "disp-3",
        None,
    );
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-00000003",
        "disp-4",
        None,
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();

    let fetch = |_agent: &str, dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
        let dispatch = dispatch.to_string();
        async move {
            match dispatch.as_str() {
                "disp-4" => Err(anyhow::Error::msg("daemon unreachable")),
                "disp-3" => Ok(status(RunState::Working)),
                _ => Ok(status(RunState::Completed)),
            }
        }
        .boxed()
    };
    let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert_eq!(notified, 2, "only terminal runs notify");

    let rows = delegation_rows(&tmp);
    assert_eq!(rows.len(), 2, "two agents, same request id, two rows");
    assert_eq!(rows[0]["source_id"], "butler\u{1f}req-00000001");
    assert_eq!(rows[1]["source_id"], "maid\u{1f}req-00000001");
    assert_eq!(rows[0]["event_id"], "completed\u{1f}thread\u{1f}-");
    assert_eq!(rows[0]["delivery_state"], "accepted");

    // Re-observation is suppressed by the binding's notified marker even
    // if the outbox's own dedup receipts were evicted entirely.
    let evicted = BridgeOutbox::shared(tmp.path()).unwrap();
    let _ = evicted; // rows deleted below through a fresh connection
    {
        let path = tmp.path().join("sessions").join("bridge_outbox.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM bridge_outbox", []).unwrap();
    }
    let again = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert_eq!(again, 0, "notified marker outlives outbox retention");
    assert!(delegation_rows(&tmp).is_empty());

    // A later, different outcome on the same request is a new event.
    let fetch_failed =
        |_agent: &str, dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
            let dispatch = dispatch.to_string();
            async move {
                if dispatch == "disp-1" {
                    Ok(status(RunState::Failed))
                } else {
                    Err(anyhow::Error::msg("skip others"))
                }
            }
            .boxed()
        };
    reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch_failed)
        .await
        .unwrap();
    let rows = delegation_rows(&tmp);
    assert_eq!(
        rows.len(),
        1,
        "distinct outcome is a distinct canonical event"
    );
    assert_eq!(rows[0]["source_id"], "butler\u{1f}req-00000001");
    assert_eq!(rows[0]["event_id"], "failed\u{1f}thread\u{1f}-");
}

#[tokio::test]
async fn stale_route_is_never_queried_and_pagination_wraps() {
    let (tmp, config) = fixture_config();
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    // Stale route: the observer must refuse to QUERY this dispatch at
    // all (not merely skip the enqueue) — proven by a fetch counter.
    seed_binding(
        &backend,
        &config,
        "butler",
        "req-stale",
        "disp-stale",
        Some("stale"),
    );
    // Fresh bindings; page size 2 forces multi-page coverage.
    for index in 1..=3 {
        seed_binding(
            &backend,
            &config,
            "butler",
            &format!("req-p{index}"),
            &format!("disp-p{index}"),
            None,
        );
    }
    let shared = RwLock::new(config);
    let stop = tokio_util::sync::CancellationToken::new();
    let queried = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut rotation = Rotation {
        cursor: 0,
        page_size: 2,
        full_pages_since_wrap: 0,
        sweep_max_rowid: 0,
    };
    let counter = std::sync::Arc::clone(&queried);
    let fetch =
        move |_agent: &str, _dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { Ok(status(RunState::Completed)) }.boxed()
        };
    // First page: rows 1-2 (stale first, then p1). The stale dispatch
    // must not be counted.
    let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert_eq!(notified, 1);
    assert_eq!(queried.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(rotation.cursor > 0, "full page advances the cursor");

    // Second page: p2 AND p3 (page size 2) — both terminal, both notified.
    let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert_eq!(notified, 2, "p2 and p3 terminal on the second page");
    assert!(rotation.cursor > 0);
    // Third pass: page past the end is short → wraps to 0, nothing new.
    let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert_eq!(notified, 0, "short page wraps with nothing left");
    assert_eq!(rotation.cursor, 0, "short page resets the cursor");
    // Fourth pass from 0: (stale, p1) — stale is NEVER queried; p1 is
    // queried but marker-suppressed.
    let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert_eq!(notified, 0, "stale row never notifies; p1 already marked");
    // Total queries: p1 once per sweep-window visit + p2 + p3 — never stale.
    assert_eq!(queried.load(std::sync::atomic::Ordering::SeqCst), 4);
    // Wait: p3's second pass queried p3 only (rows after cursor 4 = stale? no:
    // rows are (stale, p1, p2, p3) by rowid; page from cursor>0 picks the rest.
    let rows = delegation_rows(&tmp);
    assert_eq!(rows.len(), 3, "three fresh terminals, stale absent");
    assert!(
        !rows
            .iter()
            .any(|row| row["source_id"].as_str().unwrap().contains("req-stale"))
    );
}

#[tokio::test]
async fn static_table_walks_to_full_depth_and_churn_forces_the_rewind() {
    let (tmp, config) = fixture_config();
    let shared = RwLock::new(config);
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    for index in 0..18 {
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            &format!("req-r{index:02}"),
            &format!("disp-r{index:02}"),
            None,
        );
    }
    let stop = tokio_util::sync::CancellationToken::new();
    let queries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut rotation = Rotation {
        cursor: 0,
        page_size: 2,
        full_pages_since_wrap: 0,
        sweep_max_rowid: 0,
    };
    let counter = std::sync::Arc::clone(&queries);
    let fetch =
        move |_agent: &str, _dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(status(RunState::Completed)) }.boxed()
        };

    // STATIC table, 9 full pages: the rewind must NOT fire — the sweep
    // walks every page to its natural end regardless of depth.
    for _ in 0..9 {
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert!(rotation.cursor > 0, "static pages always advance");
    }
    assert_eq!(
        queries.load(std::sync::atomic::Ordering::SeqCst),
        18,
        "all 18 rows covered — the tail is reachable at any depth"
    );
    let _ = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch).await;
    assert_eq!(rotation.cursor, 0, "the empty page wraps naturally");

    // CHURN: with the counter already high, insert rows BEYOND the
    // sweep's original extent — the next full page must force the wrap.
    for index in 18..22 {
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            &format!("req-c{index:02}"),
            &format!("disp-c{index:02}"),
            None,
        );
    }
    // One pass re-walks from the top (fresh sweep, extent 22).
    reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert!(rotation.sweep_max_rowid > 0);
    // Simulate the sweep being outrun: park the cursor AT the sweep's
    // original extent, set the counter at the threshold, and insert
    // beyond it — the next page is entirely churn.
    rotation.cursor = rotation.sweep_max_rowid;
    rotation.full_pages_since_wrap = REWIND_PASSES;
    for index in 22..26 {
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            &format!("req-x{index:02}"),
            &format!("disp-x{index:02}"),
            None,
        );
    }
    reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    assert_eq!(
        rotation.cursor, 0,
        "a full page beyond the sweep extent forces the rewind"
    );
    assert_eq!(rotation.full_pages_since_wrap, 0);
}

#[tokio::test]
async fn target_flip_a_to_b_to_a_stays_silent_after_retention_loss() {
    let (tmp, config) = fixture_config();
    let shared = RwLock::new(config.clone());
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-flip",
        "disp-flip",
        None,
    );
    let stop = tokio_util::sync::CancellationToken::new();

    async fn notify_once(
        config: Config,
        tmp: &tempfile::TempDir,
        stop: &tokio_util::sync::CancellationToken,
    ) -> usize {
        let shared = RwLock::new(config);
        let mut rotation = Rotation::default();
        // Evict the outbox before every observation: only the marker
        // list can suppress re-notification. Ensure the schema exists
        // first (the very first eviction precedes any enqueue).
        BridgeOutbox::shared(tmp.path()).unwrap();
        let path = tmp.path().join("sessions").join("bridge_outbox.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM bridge_outbox", []).unwrap();
        drop(conn);
        reconcile(&shared, tmp.path(), stop, &mut rotation, &|a, d, r| {
            completed_fetch(a, d, r)
        })
        .await
        .unwrap()
    }

    // A → B → A with eviction each time: A once, B once, A silent.
    assert_eq!(
        notify_once(config.clone(), &tmp, &stop).await,
        1,
        "A notified"
    );
    let mut to_b = config.clone();
    to_b.tachi
        .result_notification
        .as_mut()
        .expect("target")
        .recipient = "owner-b".into();
    assert_eq!(
        notify_once(to_b, &tmp, &stop).await,
        1,
        "B notified (new destination)"
    );
    assert_eq!(
        notify_once(config, &tmp, &stop).await,
        0,
        "A stays silent: remembered across the flip despite eviction"
    );
}

#[tokio::test]
async fn target_change_re_notifies_the_new_destination() {
    let (tmp, config) = fixture_config();
    let shared = RwLock::new(config.clone());
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-t1",
        "disp-t1",
        None,
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();
    // First destination notified + marked.
    reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
        completed_fetch(a, d, r)
    })
    .await
    .unwrap();
    assert_eq!(delegation_rows(&tmp).len(), 1);

    // Evict the outbox entirely (retention loss) AND change the target:
    // the destination-scoped marker must let the new target be notified.
    {
        let path = tmp.path().join("sessions").join("bridge_outbox.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM bridge_outbox", []).unwrap();
    }
    let mut retargeted = config;
    retargeted
        .tachi
        .result_notification
        .as_mut()
        .expect("target")
        .recipient = "owner-new".into();
    let shared = RwLock::new(retargeted);
    let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
        completed_fetch(a, d, r)
    })
    .await
    .unwrap();
    assert_eq!(notified, 1, "a new destination is a new notification");
    let rows = delegation_rows(&tmp);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["recipient"], "owner-new");

    // Same destination again after another eviction: suppressed by the
    // now-destination-scoped marker.
    {
        let path = tmp.path().join("sessions").join("bridge_outbox.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM bridge_outbox", []).unwrap();
    }
    let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
        completed_fetch(a, d, r)
    })
    .await
    .unwrap();
    assert_eq!(
        notified, 0,
        "same destination stays suppressed past eviction"
    );
    assert!(delegation_rows(&tmp).is_empty());
}

fn with_thread(config: &Config, thread: Option<&str>) -> Config {
    let mut next = config.clone();
    next.tachi
        .result_notification
        .as_mut()
        .expect("target")
        .thread_id = thread.map(str::to_string);
    next
}

fn deliverable(config: &Config, tmp: &tempfile::TempDir) -> Vec<Option<String>> {
    let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
    outbox
        .pending("tg", 0, 100)
        .unwrap()
        .into_iter()
        .filter(|row| row.source_kind == DELEGATION && permits_delivery(config, &outbox, row))
        .map(|row| row.thread_id)
        .collect()
}

#[tokio::test]
async fn queued_notice_retargeted_to_another_thread_same_recipient_is_not_lost() {
    // Regression (#566 review): notice queued for thread A, NOT yet
    // delivered; the target moves to thread B of the SAME recipient.
    // A is withheld by permits_delivery, so B must get its own row —
    // neither the notified marker nor the outbox dedup may suppress it.
    let (tmp, base) = fixture_config();
    let thread_a = with_thread(&base, Some("thread-a"));
    let thread_b = with_thread(&base, Some("thread-b"));
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(&backend, &thread_a, "butler", "req-th", "disp-th", None);
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();
    let fetch =
        |a: &str, d: &str, r: &zeroclaw_config::tachi::TachiConfig| completed_fetch(a, d, r);

    let shared = RwLock::new(thread_a.clone());
    assert_eq!(
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap(),
        1
    );
    assert_eq!(deliverable(&thread_a, &tmp), vec![Some("thread-a".into())]);

    // Retarget to thread B before A was delivered.
    let shared = RwLock::new(thread_b.clone());
    assert_eq!(
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap(),
        1,
        "thread B is a new destination"
    );
    assert_eq!(
        deliverable(&thread_b, &tmp),
        vec![Some("thread-b".into())],
        "exactly the B notice is deliverable under the B target"
    );
    let rows = delegation_rows(&tmp);
    assert_eq!(rows.len(), 2, "A and B rows coexist");
    assert_ne!(rows[0]["event_id"], rows[1]["event_id"]);

    // Steady state at B: no further rows.
    assert_eq!(
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap(),
        0
    );
    // Flip back to A: already marked → no new row, and the still-queued
    // A row becomes deliverable again (exactly one A notice).
    let shared = RwLock::new(thread_a.clone());
    assert_eq!(
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap(),
        0
    );
    assert_eq!(deliverable(&thread_a, &tmp), vec![Some("thread-a".into())]);
    assert_eq!(delegation_rows(&tmp).len(), 2);
}

#[tokio::test]
async fn thread_less_and_threaded_targets_are_distinct_destinations() {
    let (tmp, base) = fixture_config();
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(&backend, &base, "butler", "req-nt", "disp-nt", None);
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();
    let fetch =
        |a: &str, d: &str, r: &zeroclaw_config::tachi::TachiConfig| completed_fetch(a, d, r);
    let shared = RwLock::new(base.clone());
    reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
        .await
        .unwrap();
    // New thread-less events cannot collide with a legacy threaded row.
    assert_eq!(
        delegation_rows(&tmp)[0]["event_id"],
        "completed\u{1f}thread\u{1f}-"
    );
    let shared = RwLock::new(with_thread(&base, Some("t")));
    assert_eq!(
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap(),
        1
    );
}

#[test]
fn marker_destinations_are_thread_exact() {
    let list = marker_push("", &marker_entry("completed", "tg", "owner", Some("a")));
    assert!(already_notified(
        &list,
        "completed",
        "tg",
        "owner",
        Some("a")
    ));
    assert!(!already_notified(
        &list,
        "completed",
        "tg",
        "owner",
        Some("b")
    ));
    assert!(!already_notified(&list, "completed", "tg", "owner", None));
    // Empty thread is distinct from no thread.
    assert_ne!(
        marker_entry("completed", "tg", "owner", Some("")),
        marker_entry("completed", "tg", "owner", None)
    );
    assert_ne!(
        destination_event_id("completed", Some("")),
        destination_event_id("completed", None)
    );
}

fn legacy_notice_fixture(
    thread: Option<&str>,
    marked: bool,
) -> (tempfile::TempDir, Config, String, i64) {
    let (tmp, base) = fixture_config();
    let config = with_thread(&base, thread);
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(
        &backend,
        &config,
        "butler",
        "legacy-request",
        "legacy-dispatch",
        None,
    );
    let rowid = backend.list_dispatch_bindings_page(0, 1).unwrap()[0].rowid;
    let id = BridgeOutbox::shared(tmp.path())
        .unwrap()
        .enqueue_source(
            "tg",
            "owner",
            thread,
            "legacy notice",
            DELEGATION,
            "butler\u{1f}legacy-request",
            "completed",
        )
        .unwrap();
    if marked {
        backend
            .mark_binding_notified(rowid, "completed\u{1f}tg\u{1f}owner")
            .unwrap();
    }
    (tmp, config, id, rowid)
}

async fn observe(config: &Config, tmp: &tempfile::TempDir) -> usize {
    reconcile(
        &RwLock::new(config.clone()),
        tmp.path(),
        &tokio_util::sync::CancellationToken::new(),
        &mut Rotation::default(),
        &completed_fetch,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn legacy_pending_receipt_recovers_thread_before_retarget() {
    for (old_thread, new_thread) in [
        (Some("thread-a"), Some("thread-b")),
        (Some("thread-a"), None),
        (None, Some("thread-b")),
    ] {
        let (tmp, original, old_id, rowid) = legacy_notice_fixture(old_thread, true);
        let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
        let old_row = outbox.pending("tg", 0, 100).unwrap().remove(0);
        let retargeted = with_thread(&original, new_thread);

        assert_eq!(observe(&retargeted, &tmp).await, 1);
        assert_eq!(
            deliverable(&retargeted, &tmp),
            vec![new_thread.map(str::to_string)],
            "the new target has one deliverable notice, including a target with no thread"
        );
        let rows = outbox.pending("tg", 0, 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().find(|row| row.id == old_id), Some(&old_row));
        assert_ne!(rows[0].event_id, rows[1].event_id);
        assert_eq!(observe(&retargeted, &tmp).await, 0);
        assert_eq!(observe(&original, &tmp).await, 0);
        assert_eq!(
            deliverable(&original, &tmp),
            vec![old_thread.map(str::to_string)],
            "returning to the old target resumes its original pending row"
        );

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let marker = backend.binding_notified_marker(rowid).unwrap().unwrap();
        assert!(!marker_contains(&marker, "completed\u{1f}tg\u{1f}owner"));
        for thread in [old_thread, new_thread] {
            assert!(already_notified(
                &marker,
                "completed",
                "tg",
                "owner",
                thread
            ));
        }

        // Once the old destination is recovered, retention cannot turn
        // its legacy marker back into a wildcard over future destinations.
        assert!(
            outbox
                .claim(
                    "tg",
                    &old_id,
                    zeroclaw_api::time::epoch_second(jiff::Timestamp::now())
                )
                .unwrap()
        );
        outbox.mark_sent("tg", &old_id).unwrap();
        assert!(outbox.ack("tg", &old_id).unwrap());
        let connection =
            rusqlite::Connection::open(tmp.path().join("sessions").join("bridge_outbox.db"))
                .unwrap();
        connection
            .execute("DELETE FROM bridge_outbox WHERE id = ?1", [&old_id])
            .unwrap();
        assert_eq!(observe(&original, &tmp).await, 0);
        assert_eq!(
            observe(&with_thread(&original, Some("thread-c")), &tmp).await,
            1
        );
    }
}

#[tokio::test]
async fn legacy_attempts_and_owner_actions_survive_recovery_without_replay() {
    for state in [
        "accepted",
        "sent",
        "unknown",
        "confirmed",
        "dismissed",
        "expired",
    ] {
        // With no marker, this is the crash-after-enqueue window. With
        // the old marker, it is an ordinary pre-upgrade notification.
        for marked in [false, true] {
            let (tmp, original, old_id, _) = legacy_notice_fixture(Some("thread-a"), marked);
            let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
            let source_id = "butler\u{1f}legacy-request";
            let now = zeroclaw_api::time::epoch_second(jiff::Timestamp::now());
            match state {
                "accepted" => {
                    outbox
                        .owner_action(
                            "tg",
                            "owner",
                            DELEGATION,
                            source_id,
                            &old_id,
                            "snooze",
                            Some(now + 3600),
                            "operator",
                        )
                        .unwrap();
                }
                "sent" | "unknown" | "confirmed" => {
                    assert!(outbox.claim("tg", &old_id, now).unwrap());
                    if state != "unknown" {
                        outbox.mark_sent("tg", &old_id).unwrap();
                    }
                    if state == "confirmed" {
                        assert!(outbox.ack("tg", &old_id).unwrap());
                    }
                }
                "dismissed" => {
                    outbox
                        .owner_action(
                            "tg", "owner", DELEGATION, source_id, &old_id, "dismiss", None,
                            "operator",
                        )
                        .unwrap();
                }
                "expired" => {
                    let connection = rusqlite::Connection::open(
                        tmp.path().join("sessions").join("bridge_outbox.db"),
                    )
                    .unwrap();
                    connection
                        .execute(
                            "UPDATE bridge_outbox SET expires_at = ?1 WHERE id = ?2",
                            rusqlite::params![now - 1, &old_id],
                        )
                        .unwrap();
                    outbox.purge_expired().unwrap();
                }
                _ => unreachable!(),
            }
            outbox
                .owner_action(
                    "tg", "owner", DELEGATION, source_id, &old_id, "mute", None, "operator",
                )
                .unwrap();
            let before = outbox.inspect("tg", &old_id).unwrap().unwrap();
            assert_eq!(before["delivery_state"], state);
            assert_eq!(observe(&original, &tmp).await, 0);
            assert_eq!(
                delegation_rows(&tmp).len(),
                1,
                "a recovered receipt never gets replayed"
            );
            assert_eq!(outbox.inspect("tg", &old_id).unwrap().unwrap(), before);

            let retargeted = with_thread(&original, Some("thread-b"));
            assert_eq!(observe(&retargeted, &tmp).await, 1);
            assert_eq!(outbox.inspect("tg", &old_id).unwrap().unwrap(), before);
            let new_row = outbox
                .pending("tg", 0, 100)
                .unwrap()
                .into_iter()
                .find(|row| row.thread_id.as_deref() == Some("thread-b"))
                .unwrap();
            assert!(
                !outbox.claim("tg", &new_row.id, now).unwrap(),
                "source mute still applies at the new thread"
            );
            assert_eq!(observe(&original, &tmp).await, 0);
        }
    }
}

#[tokio::test]
async fn legacy_marker_without_receipt_keeps_conservative_suppression() {
    let (tmp, original, old_id, _) = legacy_notice_fixture(Some("thread-a"), true);
    let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
    assert!(
        outbox
            .claim(
                "tg",
                &old_id,
                zeroclaw_api::time::epoch_second(jiff::Timestamp::now())
            )
            .unwrap()
    );
    outbox.mark_sent("tg", &old_id).unwrap();
    assert!(outbox.ack("tg", &old_id).unwrap());
    let connection =
        rusqlite::Connection::open(tmp.path().join("sessions").join("bridge_outbox.db")).unwrap();
    connection.execute("DELETE FROM bridge_outbox", []).unwrap();
    for thread in [Some("thread-a"), Some("thread-b"), None] {
        assert_eq!(observe(&with_thread(&original, thread), &tmp).await, 0);
    }
    assert!(delegation_rows(&tmp).is_empty());
    // Evidence was lost only for this recipient/outcome, not all targets.
    let mut new_recipient = original;
    new_recipient
        .tachi
        .result_notification
        .as_mut()
        .unwrap()
        .recipient = "owner-new".into();
    assert_eq!(observe(&new_recipient, &tmp).await, 1);
}

#[tokio::test]
async fn legacy_receipt_read_failure_preserves_prior_enqueue_markers() {
    let (tmp, config) = fixture_config();
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    for request in ["first-request", "legacy-request"] {
        seed_binding(&backend, &config, "butler", request, request, None);
    }
    let bindings = backend.list_dispatch_bindings_page(0, 10).unwrap();
    backend
        .mark_binding_notified(bindings[1].rowid, "completed\u{1f}tg\u{1f}owner")
        .unwrap();
    let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
    let old_id = outbox
        .enqueue_source(
            "tg",
            "owner",
            Some("thread-a"),
            "legacy notice",
            DELEGATION,
            "butler\u{1f}legacy-request",
            "completed",
        )
        .unwrap();
    let connection =
        rusqlite::Connection::open(tmp.path().join("sessions").join("bridge_outbox.db")).unwrap();
    connection
        .execute(
            "UPDATE bridge_outbox SET thread_id = x'80' WHERE id = ?1",
            [&old_id],
        )
        .unwrap();
    let result = reconcile(
        &RwLock::new(config),
        tmp.path(),
        &tokio_util::sync::CancellationToken::new(),
        &mut Rotation::default(),
        &completed_fetch,
    )
    .await;
    assert!(
        result.is_err(),
        "a malformed receipt is not treated as missing"
    );
    assert_eq!(delegation_rows(&tmp).len(), 2);
    let first = backend
        .binding_notified_marker(bindings[0].rowid)
        .unwrap()
        .unwrap();
    assert!(already_notified(&first, "completed", "tg", "owner", None));
    assert_eq!(
        backend
            .binding_notified_marker(bindings[1].rowid)
            .unwrap()
            .as_deref(),
        Some("completed\u{1f}tg\u{1f}owner"),
        "failed recovery must not erase the legacy history"
    );
}

#[tokio::test]
async fn disabled_agent_and_uneigible_config_withhold_notifications() {
    let (tmp, mut config) = fixture_config();
    let shared = RwLock::new(config.clone());
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-00000001",
        "disp-1",
        None,
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();

    // Agent disabled, section still eligible: zero enqueues.
    config.agents.insert(
        "butler".into(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: false,
            ..Default::default()
        },
    );
    let shared = RwLock::new(config.clone());
    assert_eq!(
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
            completed_fetch(a, d, r)
        })
        .await
        .unwrap(),
        0
    );

    // Agent re-enabled, section disabled: still zero, no error.
    config.agents.insert("butler".into(), Default::default());
    config.tachi.enabled = false;
    let shared = RwLock::new(config);
    assert_eq!(
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
            completed_fetch(a, d, r)
        })
        .await
        .unwrap(),
        0
    );
}

#[test]
fn permits_delivery_gates_on_owning_agent_and_section() {
    let (tmp, mut config) = fixture_config();
    let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
    let row = |source_id: &str, bridge: &str, to: &str| zeroclaw_infra::bridge_outbox::OutboxItem {
        seq: 1,
        id: "id".into(),
        bridge: bridge.into(),
        to: to.into(),
        thread_id: None,
        content: String::new(),
        source_kind: DELEGATION.into(),
        source_id: source_id.into(),
        event_id: "completed".into(),
        delivery_state: "accepted".into(),
        snoozed_until: None,
        expires_at: 0,
    };
    assert!(permits_delivery(
        &config,
        &outbox,
        &row("butler\u{1f}req-1", "tg", "owner")
    ));
    // Wrong owner scoping: an agent that does not exist.
    assert!(!permits_delivery(
        &config,
        &outbox,
        &row("ghost\u{1f}req-1", "tg", "owner")
    ));
    // A malformed or manually stored composite owner cannot borrow an
    // enabled prefix's authority. Request ids contain no separator.
    assert!(!permits_delivery(
        &config,
        &outbox,
        &row("butler\u{1f}ghost\u{1f}req-1", "tg", "owner")
    ));
    // Disabled owner withholds even though the request ran.
    config.agents.insert(
        "butler".into(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: false,
            ..Default::default()
        },
    );
    assert!(!permits_delivery(
        &config,
        &outbox,
        &row("butler\u{1f}req-1", "tg", "owner")
    ));
    config.agents.insert("butler".into(), Default::default());
    // Section off withholds queued-unsent rows.
    config.tachi.enabled = false;
    assert!(!permits_delivery(
        &config,
        &outbox,
        &row("butler\u{1f}req-1", "tg", "owner")
    ));
    config.tachi.enabled = true;
    // Mismatched destination withholds.
    assert!(!permits_delivery(
        &config,
        &outbox,
        &row("butler\u{1f}req-1", "tg", "other")
    ));
}

#[test]
fn partial_input_required_is_attention_worthy() {
    let mut partial = status(RunState::InputRequired);
    partial.closure_kind = Some("partial".into());
    assert_eq!(terminal_outcome(&partial), Some("partial"));
    let blocked = status(RunState::InputRequired);
    assert_eq!(terminal_outcome(&blocked), None);
    assert_eq!(terminal_outcome(&status(RunState::Submitted)), None);
}

struct RecordingMemory {
    episodes: parking_lot::Mutex<Vec<EpisodeRecord>>,
    recall_entries: parking_lot::Mutex<Vec<zeroclaw_api::memory_traits::MemoryEntry>>,
    fail_commit: bool,
}

impl RecordingMemory {
    fn new(fail_commit: bool) -> Self {
        Self {
            episodes: parking_lot::Mutex::new(Vec::new()),
            recall_entries: parking_lot::Mutex::new(Vec::new()),
            fail_commit,
        }
    }
}

impl zeroclaw_api::attribution::Attributable for RecordingMemory {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Memory(zeroclaw_api::attribution::MemoryKind::None)
    }
    fn alias(&self) -> &str {
        "recording_memory"
    }
}

#[async_trait::async_trait]
impl Memory for RecordingMemory {
    fn name(&self) -> &str {
        "recording"
    }
    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: zeroclaw_api::memory_traits::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<zeroclaw_api::memory_traits::MemoryEntry>> {
        Ok(self.recall_entries.lock().clone())
    }
    async fn get(
        &self,
        _key: &str,
    ) -> anyhow::Result<Option<zeroclaw_api::memory_traits::MemoryEntry>> {
        Ok(None)
    }
    async fn list(
        &self,
        _category: Option<&zeroclaw_api::memory_traits::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<zeroclaw_api::memory_traits::MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn health_check(&self) -> bool {
        true
    }
    async fn store_with_agent(
        &self,
        _key: &str,
        _content: &str,
        _category: zeroclaw_api::memory_traits::MemoryCategory,
        _session_id: Option<&str>,
        _namespace: Option<&str>,
        _importance: Option<f64>,
        _agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn recall_for_agents(
        &self,
        _allowed_agent_ids: &[&str],
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<zeroclaw_api::memory_traits::MemoryEntry>> {
        Ok(self.recall_entries.lock().clone())
    }
    async fn commit_episode(&self, episode: &EpisodeRecord) -> anyhow::Result<String> {
        if self.fail_commit {
            anyhow::bail!("simulated episode commit failure");
        }
        episode.validate()?;
        self.episodes.lock().push(episode.clone());
        Ok(episode.episode_id.clone())
    }
    async fn read_episodes_in_window(
        &self,
        agent_id: &str,
        _since: Option<&str>,
        _until: Option<&str>,
        _session_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<EpisodeRecord>> {
        Ok(self
            .episodes
            .lock()
            .iter()
            .filter(|ep| ep.agent_id == agent_id)
            .take(limit)
            .cloned()
            .collect())
    }
}

struct ScriptedModelProvider {
    reply: String,
    seen_prompts: parking_lot::Mutex<Vec<(Option<String>, String)>>,
}

impl ScriptedModelProvider {
    fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            seen_prompts: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

impl zeroclaw_api::attribution::Attributable for ScriptedModelProvider {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Provider(zeroclaw_api::attribution::ProviderKind::Model(
            zeroclaw_api::attribution::ModelProviderKind::OpenAi,
        ))
    }
    fn alias(&self) -> &str {
        "scripted_provider"
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedModelProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        self.seen_prompts
            .lock()
            .push((system_prompt.map(str::to_string), message.to_string()));
        Ok(self.reply.clone())
    }
    async fn chat_with_history(
        &self,
        _messages: &[zeroclaw_api::model_provider::ChatMessage],
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        Ok(self.reply.clone())
    }
}

struct FixedResolver(Option<String>);

#[async_trait::async_trait]
impl ContinuationResolver for FixedResolver {
    async fn resolve(&self, _ctx: &ContinuationContext<'_>) -> anyhow::Result<Option<String>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn input_required_auth_required_hard_escalates_once_without_calling_resolver() {
    let (tmp, config) = fixture_config();
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(&backend, &config, "butler", "req-auth", "disp-auth", None);
    let shared = RwLock::new(config);
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();

    let resolver_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct PanicIfCalled(Arc<std::sync::atomic::AtomicBool>);
    #[async_trait::async_trait]
    impl ContinuationResolver for PanicIfCalled {
        async fn resolve(&self, _ctx: &ContinuationContext<'_>) -> anyhow::Result<Option<String>> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Some("should not run".into()))
        }
    }
    let resolver = PanicIfCalled(Arc::clone(&resolver_called));

    let fetch_status = |_a: &str, _d: &str, _r: &zeroclaw_config::tachi::TachiConfig| {
        async {
            Ok(RunStatus {
                dispatch_id: "disp-auth".into(),
                state: RunState::InputRequired,
                revision: Some(1),
                closure_kind: None,
                result_written: None,
                updated_at: None,
                read_projection: Some(
                    serde_json::json!({"auth_required": true, "question": "Approve OAuth?"}),
                ),
            })
        }
        .boxed()
    };
    let send_continue =
        |_a: &str, _d: &str, _r: &zeroclaw_config::tachi::TachiConfig, _rev: u64, _ans: &str| {
            async { panic!("AUTH_REQUIRED must never invoke continue_run") }.boxed()
        };
    let deps = SweepDeps {
        fetch_status: &fetch_status,
        fetch_result: None,
        send_continue: Some(&send_continue),
        continuation_resolver: Some(&resolver),
        memory: None,
    };

    assert_eq!(
        reconcile_with_deps(&shared, tmp.path(), &stop, &mut rotation, &deps)
            .await
            .unwrap(),
        1
    );
    assert!(!resolver_called.load(std::sync::atomic::Ordering::SeqCst));
    let rows = delegation_rows(&tmp);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["event_id"], "input_required\u{1f}thread\u{1f}-");
    let pending = BridgeOutbox::shared(tmp.path())
        .unwrap()
        .pending("tg", 0, 100)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].content.contains("AUTH_REQUIRED"));

    // Second pass stays deduplicated.
    assert_eq!(
        reconcile_with_deps(&shared, tmp.path(), &stop, &mut rotation, &deps)
            .await
            .unwrap(),
        0
    );
    assert_eq!(delegation_rows(&tmp).len(), 1);
}

#[tokio::test]
async fn grounded_proxy_continuation_tracks_three_rounds_prevents_same_revision_replay_and_escalates_on_cap()
 {
    let (tmp, config) = fixture_config();
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(&backend, &config, "butler", "req-loop", "disp-loop", None);
    let shared = RwLock::new(config);
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();

    let current_rev = Arc::new(std::sync::atomic::AtomicU64::new(10));
    let continue_calls = Arc::new(parking_lot::Mutex::new(Vec::<(u64, String)>::new()));
    let resolver = FixedResolver(Some("Keep snake_case SQLite column names.".into()));

    let rev_for_status = Arc::clone(&current_rev);
    let fetch_status = move |_a: &str, _d: &str, _r: &zeroclaw_config::tachi::TachiConfig| {
        let rev = rev_for_status.load(std::sync::atomic::Ordering::SeqCst);
        async move {
            Ok(RunStatus {
                dispatch_id: "disp-loop".into(),
                state: RunState::InputRequired,
                revision: Some(rev),
                closure_kind: None,
                result_written: None,
                updated_at: None,
                read_projection: Some(serde_json::json!({"question": "Column naming convention?"})),
            })
        }
        .boxed()
    };
    let calls_ref = Arc::clone(&continue_calls);
    let rev_for_continue = Arc::clone(&current_rev);
    let send_continue = move |_a: &str,
                              _d: &str,
                              _r: &zeroclaw_config::tachi::TachiConfig,
                              expected_rev: u64,
                              answer: &str| {
        calls_ref.lock().push((expected_rev, answer.to_string()));
        let next_rev = expected_rev + 1;
        rev_for_continue.store(next_rev, std::sync::atomic::Ordering::SeqCst);
        async move {
            Ok(ContinueReceipt {
                dispatch_id: "disp-loop".into(),
                state: RunState::Working,
                status_revision: Some(next_rev),
            })
        }
        .boxed()
    };
    let deps = SweepDeps {
        fetch_status: &fetch_status,
        fetch_result: None,
        send_continue: Some(&send_continue),
        continuation_resolver: Some(&resolver),
        memory: None,
    };

    // Rounds 1, 2, 3 succeed and enqueue transparent audit notes.
    for expected_round in 1..=3u8 {
        assert_eq!(
            reconcile_with_deps(&shared, tmp.path(), &stop, &mut rotation, &deps)
                .await
                .unwrap(),
            1
        );
        let row = backend
            .list_dispatch_bindings_page(0, 10)
            .unwrap()
            .remove(0);
        assert_eq!(row.proxy_continuations, expected_round);
        assert_eq!(row.last_proxy_revision, Some(9 + u64::from(expected_round)));
    }
    assert_eq!(continue_calls.lock().len(), 3);

    // Round 4 hits the cap -> escalates once via input_required without silencing.
    assert_eq!(
        reconcile_with_deps(&shared, tmp.path(), &stop, &mut rotation, &deps)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        continue_calls.lock().len(),
        3,
        "cap blocks 4th continue_run"
    );
    let rows = delegation_rows(&tmp);
    assert_eq!(rows.len(), 4, "3 audit notes + 1 input_required escalation");
    assert_eq!(rows[3]["event_id"], "input_required\u{1f}thread\u{1f}-");

    // Same-revision idempotency check: if revision is still at a previously
    // answered revision, a new dispatch with count=1 and last_proxy_revision=Some(20)
    // must NOT call continue_run again for rev=20.
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-same-rev",
        "disp-same-rev",
        None,
    );
    let same_row = backend
        .list_dispatch_bindings_page(0, 10)
        .unwrap()
        .into_iter()
        .find(|r| r.request_id == "req-same-rev")
        .unwrap();
    assert!(
        backend
            .record_proxy_continuation(same_row.rowid, 20, 1)
            .unwrap()
    );
    let fetch_same = |_a: &str, d: &str, _r: &zeroclaw_config::tachi::TachiConfig| {
        let d = d.to_string();
        async move {
            if d == "disp-same-rev" {
                Ok(RunStatus {
                    dispatch_id: d,
                    state: RunState::InputRequired,
                    revision: Some(20),
                    closure_kind: None,
                    result_written: None,
                    updated_at: None,
                    read_projection: Some(serde_json::json!({"prompt": "Confirm?"})),
                })
            } else {
                Err(anyhow::Error::msg("skip"))
            }
        }
        .boxed()
    };
    let same_deps = SweepDeps {
        fetch_status: &fetch_same,
        fetch_result: None,
        send_continue: Some(&send_continue),
        continuation_resolver: Some(&resolver),
        memory: None,
    };
    assert_eq!(
        reconcile_with_deps(&shared, tmp.path(), &stop, &mut rotation, &same_deps)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        continue_calls.lock().len(),
        3,
        "same revision was not re-answered"
    );
}

#[tokio::test]
async fn grounded_continuation_resolver_uses_session_and_non_soul_memory_and_returns_none_when_insufficient()
 {
    let (tmp, config) = fixture_config();
    // Seed Soul and UserModel databases in data_dir with marker strings to
    // prove GroundedContinuationResolver never reads or emits them.
    zeroclaw_memory::companion::SoulProfileStore::shared(tmp.path())
        .unwrap()
        .ensure_seeded("butler", "SECRET_SOUL_BYTES", 1)
        .unwrap();
    zeroclaw_memory::companion::UserModelStore::shared(tmp.path())
        .unwrap()
        .record_owner_statement(
            zeroclaw_memory::companion::UserModelKind::Preference,
            "SECRET_USER_MODEL_BYTES",
            "pref.test",
            "global",
            1,
        )
        .unwrap();

    let mem = Arc::new(RecordingMemory::new(false));
    let provider = Arc::new(ScriptedModelProvider::new("Use WAL mode."));
    let resolver = GroundedContinuationResolver::new(
        provider.clone(),
        "test-model",
        None,
        tmp.path().to_path_buf(),
        Some(mem.clone()),
    );

    // Absent prompt -> None without calling provider.
    let no_prompt = ContinuationContext {
        agent_alias: "butler",
        request_id: "req-1",
        dispatch_id: "disp-1",
        revision: 1,
        prompt: None,
        read_projection: None,
    };
    assert_eq!(resolver.resolve(&no_prompt).await.unwrap(), None);
    assert!(provider.seen_prompts.lock().is_empty());

    // Insufficient facts (empty sessions + empty memory) -> None without calling provider.
    let with_prompt = ContinuationContext {
        agent_alias: "butler",
        request_id: "req-1",
        dispatch_id: "disp-1",
        revision: 1,
        prompt: Some("Which journal mode should SQLite use?"),
        read_projection: None,
    };
    assert_eq!(resolver.resolve(&with_prompt).await.unwrap(), None);
    assert!(provider.seen_prompts.lock().is_empty());

    // Add a Soul-namespaced memory entry (must be excluded) and a valid
    // technical EpisodeRecord + session checkpoint.
    mem.recall_entries
        .lock()
        .push(zeroclaw_api::memory_traits::MemoryEntry {
            id: "soul-1".into(),
            key: "soul::tone".into(),
            content: "SECRET_SOUL_ENTRY".into(),
            category: zeroclaw_api::memory_traits::MemoryCategory::Core,
            timestamp: "2026-11-18T00:00:00Z".into(),
            session_id: None,
            score: None,
            namespace: "soul".into(),
            importance: None,
            superseded_by: None,
            agent_alias: Some("butler".into()),
            agent_id: Some("butler".into()),
            kind: None,
            pinned: false,
            tenant_id: None,
        });
    mem.episodes.lock().push(EpisodeRecord {
        episode_id: "ep-1".into(),
        agent_id: "butler".into(),
        session_key: "sess-1".into(),
        started_at: "2026-11-18T00:00:00Z".into(),
        ended_at: "2026-11-18T01:00:00Z".into(),
        up_to_message_id: None,
        summary: "SQLite uses WAL mode for concurrent gateway reads.".into(),
        why_and_tradeoffs: vec![],
        settled_decisions: vec!["Enable PRAGMA journal_mode=WAL".into()],
        rejected_options: vec![],
        entities: vec![],
        source_kind: EpisodeSourceKind::Dialogue,
    });

    let answer = resolver.resolve(&with_prompt).await.unwrap();
    assert_eq!(answer.as_deref(), Some("Use WAL mode."));
    {
        let prompts = provider.seen_prompts.lock();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].1.contains("Enable PRAGMA journal_mode=WAL"));
        assert!(!prompts[0].1.contains("SECRET_SOUL_BYTES"));
        assert!(!prompts[0].1.contains("SECRET_USER_MODEL_BYTES"));
        assert!(!prompts[0].1.contains("SECRET_SOUL_ENTRY"));
    }

    // When provider says INSUFFICIENT_FACTS -> returns None.
    let insufficient_provider = Arc::new(ScriptedModelProvider::new("INSUFFICIENT_FACTS"));
    let insufficient_resolver = GroundedContinuationResolver::new(
        insufficient_provider,
        "test-model",
        None,
        tmp.path().to_path_buf(),
        Some(mem),
    );
    assert_eq!(
        insufficient_resolver.resolve(&with_prompt).await.unwrap(),
        None
    );
    let _ = config;
}

#[tokio::test]
async fn terminal_result_digests_episode_and_remains_fail_soft_on_result_or_commit_errors() {
    let (tmp, config) = fixture_config();
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(&backend, &config, "butler", "req-ok", "disp-ok", None);
    seed_binding(
        &backend,
        &config,
        "butler",
        "req-partial",
        "disp-partial",
        None,
    );
    seed_binding(
        &backend,
        &config,
        "butler",
        "req-nowrite",
        "disp-nowrite",
        None,
    );
    seed_binding(&backend, &config, "butler", "req-err", "disp-err", None);
    let shared = RwLock::new(config);
    let stop = tokio_util::sync::CancellationToken::new();
    let mut rotation = Rotation::default();
    let mem = Arc::new(RecordingMemory::new(false));

    let fetch_status = |_a: &str, d: &str, _r: &zeroclaw_config::tachi::TachiConfig| {
        let d = d.to_string();
        async move {
            let (state, closure_kind, result_written) = match d.as_str() {
                "disp-partial" => (RunState::InputRequired, Some("partial".into()), Some(true)),
                "disp-nowrite" => (RunState::Completed, None, Some(false)),
                _ => (RunState::Completed, None, Some(true)),
            };
            Ok(RunStatus {
                dispatch_id: d,
                state,
                revision: Some(2),
                closure_kind,
                result_written,
                updated_at: Some("2026-11-18T08:00:00Z".into()),
                read_projection: None,
            })
        }
        .boxed()
    };
    let fetch_result = |_a: &str, d: &str, _r: &zeroclaw_config::tachi::TachiConfig| {
        let d = d.to_string();
        async move {
            if d == "disp-err" {
                anyhow::bail!("result endpoint unavailable");
            }
            Ok(RunResult {
                state: RunState::Completed,
                status: status(RunState::Completed),
                task_state: None,
                state_basis: None,
                managed_run: None,
                body: Some(format!("Report for {d}: secret sk-ant-999 scrubbed")),
                truncated: false,
            })
        }
        .boxed()
    };
    let deps = SweepDeps {
        fetch_status: &fetch_status,
        fetch_result: Some(&fetch_result),
        send_continue: None,
        continuation_resolver: None,
        memory: Some(mem.as_ref()),
    };

    let notified = reconcile_with_deps(&shared, tmp.path(), &stop, &mut rotation, &deps)
        .await
        .unwrap();
    assert_eq!(notified, 4, "all 4 terminal notices still enqueue");
    assert_eq!(delegation_rows(&tmp).len(), 4);

    {
        let episodes = mem.episodes.lock();
        assert_eq!(
            episodes.len(),
            2,
            "only Completed & Partial with result_written!=false and ok fetch commit"
        );
        assert_eq!(episodes[0].episode_id, "delegation:req-ok");
        assert_eq!(episodes[0].source_kind, EpisodeSourceKind::DelegationDigest);
        assert!(episodes[0].summary.contains("[REDACTED]"));
        assert!(!episodes[0].summary.contains("sk-ant-999"));
        assert_eq!(episodes[1].episode_id, "delegation:req-partial");
    }

    // Even when memory.commit_episode fails, terminal #63 notice still delivers!
    seed_binding(
        &backend,
        &shared.read(),
        "butler",
        "req-fail-mem",
        "disp-fail-mem",
        None,
    );
    let failing_mem = RecordingMemory::new(true);
    let deps_fail_mem = SweepDeps {
        fetch_status: &fetch_status,
        fetch_result: Some(&fetch_result),
        send_continue: None,
        continuation_resolver: None,
        memory: Some(&failing_mem),
    };
    assert_eq!(
        reconcile_with_deps(&shared, tmp.path(), &stop, &mut rotation, &deps_fail_mem)
            .await
            .unwrap(),
        1
    );
    assert_eq!(delegation_rows(&tmp).len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_notify_triggers_immediate_sweep_pass() {
    let (tmp, mut config) = fixture_config();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits_srv = Arc::clone(&hits);
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(move || {
            let hits_srv = Arc::clone(&hits_srv);
            async move {
                hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    );
    let srv = ::zeroclaw_spawn::spawn!(async move {
        let _ = axum::serve(listener, app).await;
    });
    config.tachi.endpoint = format!("http://{addr}/mcp");

    let shared = Arc::new(RwLock::new(config.clone()));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let wake = Arc::new(tokio::sync::Notify::new());
    let task = start(
        Arc::clone(&shared),
        None,
        Some(Arc::clone(&wake)),
        shutdown_rx,
    );
    // Let the initial t=0 pass finish on an empty table (0 hits) and park.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

    // Seed a binding AFTER t=0 and nudge wake: sweep must run immediately
    // without waiting for the 60s CHECK_INTERVAL.
    let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
    seed_binding(&backend, &config, "butler", "req-wake", "disp-wake", None);
    task.notify();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while hits.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("wake nudge must trigger an immediate sweep within 5s");

    let _ = shutdown_tx.send(true);
    drop(task);
    srv.abort();
}
