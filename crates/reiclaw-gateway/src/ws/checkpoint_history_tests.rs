use super::*;

async fn checkpoint_client(harness: &LazyHarness) -> (RawWsClient, String) {
    let mut client =
        RawWsClient::connect(harness.address, "/ws/chat?agent=web&session_id=checkpoint").await;
    client.until("checkpoint", "session_start").await;
    client.send_json(json!({"type":"connect"})).await;
    client.until("checkpoint", "connected").await;
    client
        .send_json(json!({"type":"turn_control","version":1,"op":"negotiate"}))
        .await;
    let ready = client.until("checkpoint", "turn_ready").await;
    (client, ready["epoch"].as_str().unwrap().to_string())
}

async fn checkpoint_turn(client: &mut RawWsClient, epoch: &str, content: &str) {
    client
        .send_json(json!({
            "type":"turn_control", "version":1, "op":"message", "epoch":epoch,
            "request_id":uuid::Uuid::new_v4().to_string(), "content":content
        }))
        .await;
    let ack = client.until("checkpoint turn", "turn_ack").await;
    let turn_id = ack["turn_id"].as_str().unwrap();
    loop {
        let frame = client.read_json().await;
        assert!(
            !matches!(
                frame["type"].as_str(),
                Some("error" | "turn_error" | "turn_gap")
            ),
            "unexpected checkpoint turn error: {frame}"
        );
        if frame["type"] == "turn_event" && frame["turn_id"] == turn_id && frame["terminal"] == true
        {
            break;
        }
    }
}

fn seed_checkpoint_session(backend: &SqliteSessionBackend) -> Vec<i64> {
    backend
        .set_session_agent_alias("gw_checkpoint", "web")
        .unwrap();
    for message in [
        ChatMessage::user("covered-original-request"),
        ChatMessage::assistant("covered-original-answer"),
        ChatMessage::user("raw-delta-request"),
        ChatMessage::assistant("raw-delta-answer"),
    ] {
        backend.append("gw_checkpoint", &message).unwrap();
    }
    backend
        .load_session_window("gw_checkpoint")
        .unwrap()
        .delta_messages
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

#[tokio::test]
async fn ws_foreground_checkpoint_cold_restore_and_live_promotion_reach_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let sqlite = Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());
    let ids = seed_checkpoint_session(&sqlite);
    sqlite
        .stage_checkpoint("gw_checkpoint", ids[1], "active-original-decision")
        .unwrap();
    sqlite.promote_staged_checkpoint("gw_checkpoint").unwrap();
    sqlite
        .stage_checkpoint("gw_checkpoint", ids[3], "promoted-cumulative-decision")
        .unwrap();
    let counting = Arc::new(CountingSessionBackend::new(
        sqlite.clone(),
        std::thread::current().id(),
    ));
    let harness = spawn_lazy_harness(&tmp, Arc::clone(&counting)).await;
    let (mut client, epoch) = checkpoint_client(&harness).await;

    checkpoint_turn(&mut client, &epoch, "first-live-question").await;
    let first = harness.seen.lock()[0]["messages"].to_string();
    assert!(first.contains("active-original-decision"));
    assert!(first.contains("raw-delta-request"));
    assert!(!first.contains("covered-original-request"));
    assert!(!first.contains("promoted-cumulative-decision"));
    assert_eq!(counting.counts().0, 1);

    sqlite.promote_staged_checkpoint("gw_checkpoint").unwrap();
    checkpoint_turn(&mut client, &epoch, "second-live-question").await;
    let second = harness.seen.lock()[1]["messages"].to_string();
    assert!(second.contains("promoted-cumulative-decision"));
    assert!(second.contains("first-live-question"));
    assert!(second.contains("second-live-question"));
    assert!(!second.contains("active-original-decision"));
    assert!(!second.contains("raw-delta-request"));
    assert_eq!(counting.counts().0, 2);

    checkpoint_turn(&mut client, &epoch, "third-live-question").await;
    assert_eq!(
        counting.counts().0,
        2,
        "unchanged checkpoints must not reload history"
    );
    assert_eq!(
        counting.counts().2,
        0,
        "history reads must remain off the async thread"
    );
    assert_eq!(
        sqlite.load("gw_checkpoint").len(),
        10,
        "raw rows remain intact"
    );
    client.close().await;
}

#[tokio::test]
async fn ws_foreground_checkpoint_keeps_unsaved_live_turn_after_append_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let sqlite = Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());
    let ids = seed_checkpoint_session(&sqlite);
    let counting = Arc::new(CountingSessionBackend::new(
        sqlite.clone(),
        std::thread::current().id(),
    ));
    let harness = spawn_lazy_harness(&tmp, Arc::clone(&counting)).await;
    let (mut client, epoch) = checkpoint_client(&harness).await;
    counting.fail_next_append.store(true, Ordering::SeqCst);
    checkpoint_turn(&mut client, &epoch, "unsaved-live-question").await;
    assert_eq!(sqlite.load("gw_checkpoint").len(), 4);

    sqlite
        .stage_checkpoint("gw_checkpoint", ids[3], "new-checkpoint-after-failed-write")
        .unwrap();
    sqlite.promote_staged_checkpoint("gw_checkpoint").unwrap();
    checkpoint_turn(&mut client, &epoch, "next-live-question").await;
    let second = harness.seen.lock()[1]["messages"].to_string();
    assert!(second.contains("unsaved-live-question"));
    assert!(second.contains("next-live-question"));
    assert!(!second.contains("new-checkpoint-after-failed-write"));
    assert_eq!(
        counting.counts().0,
        1,
        "failed persistence disables live replacement"
    );
    client.close().await;
}
