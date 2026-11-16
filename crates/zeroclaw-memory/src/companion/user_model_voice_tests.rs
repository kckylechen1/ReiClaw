use super::*;

fn fact(store: &UserModelStore, key: &str, scope: &str, at: u64) -> UserModelRevision {
    store
        .record_owner_statement(
            UserModelKind::Preference,
            "Prefers concise replies.",
            key,
            scope,
            at,
        )
        .unwrap()
}

fn context() -> ApplicabilityContext {
    ApplicabilityContext::new("main", "wss", "gw_voice")
}

#[test]
fn consent_binds_revision_provider_and_cas_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserModelStore::open(dir.path()).unwrap();
    let first = fact(&store, "style", "global", 1);
    assert!(
        store
            .voice_export_heads(&context(), "moss-live", 1)
            .unwrap()
            .is_empty()
    );
    let grant = store
        .set_voice_consent(&first.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
        .unwrap();
    assert_eq!(grant.reviewer, "owner_operator");
    assert_eq!(
        store
            .voice_export_heads(&context(), "moss-live", 2)
            .unwrap(),
        std::slice::from_ref(&first)
    );
    assert!(
        store
            .voice_export_heads(&context(), "openai-realtime", 2)
            .unwrap()
            .is_empty()
    );
    drop(store);
    let store = UserModelStore::open(dir.path()).unwrap();
    assert_eq!(
        store.voice_consent(&first.id, "moss-live").unwrap(),
        Some(grant)
    );
    assert_eq!(
        store
            .voice_export_heads(&context(), "moss-live", 2)
            .unwrap(),
        std::slice::from_ref(&first)
    );
    store
        .set_voice_consent(&first.id, "moss-live", VoiceConsentAction::Revoke, 1, 3)
        .unwrap();
    assert!(
        store
            .voice_export_heads(&context(), "moss-live", 3)
            .unwrap()
            .is_empty()
    );
    assert!(is_user_model_head_conflict(
        &store
            .set_voice_consent(&first.id, "moss-live", VoiceConsentAction::Grant, 1, 3)
            .unwrap_err()
    ));
    drop(store);
    let store = UserModelStore::open(dir.path()).unwrap();
    assert!(
        store
            .voice_export_heads(&context(), "moss-live", 3)
            .unwrap()
            .is_empty()
    );
    store
        .set_voice_consent(&first.id, "moss-live", VoiceConsentAction::Grant, 2, 4)
        .unwrap();
    let second = fact(&store, "style", "global", 5);
    assert!(
        store
            .voice_export_heads(&context(), "moss-live", 5)
            .unwrap()
            .is_empty()
    );
    assert!(is_user_model_head_conflict(
        &store
            .set_voice_consent(&first.id, "moss-live", VoiceConsentAction::Grant, 3, 5)
            .unwrap_err()
    ));
    store
        .set_voice_consent(&second.id, "moss-live", VoiceConsentAction::Grant, 0, 6)
        .unwrap();
    assert_eq!(
        store
            .voice_export_heads(&context(), "moss-live", 6)
            .unwrap(),
        [second]
    );
    let conn = store.conn.lock();
    assert_eq!(
        conn.query_row::<u64, _, _>(
            "SELECT count(*) FROM user_model_voice_consent_receipts",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        4
    );
    assert!(
        conn.execute("DELETE FROM user_model_voice_consent_receipts", [])
            .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE user_model_voice_consent_receipts SET action = 'grant'",
            []
        )
        .is_err()
    );
}

#[test]
fn voice_consent_preserves_scope_and_owner_correction_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserModelStore::open(dir.path()).unwrap();
    for (key, scope) in [
        ("global", "global"),
        ("main", "agent:main"),
        ("other", "agent:other"),
        ("channel", "channel:wss"),
        ("wrong-channel", "channel:telegram"),
        ("session", "session:gw_voice"),
        ("wrong-session", "session:other"),
    ] {
        let head = fact(&store, key, scope, 1);
        store
            .set_voice_consent(&head.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
            .unwrap();
    }
    let evidence = serde_json::json!({"origin":"owner_correction", "agent":"other",
        "submitted_semantic_key":"style", "messages":[{"session_id":"gw_voice", "at_unix":1,
        "owner_text":"Please be concise.", "source":{"kind":"operator"}}]})
    .to_string();
    let candidate = store
        .record_owner_correction(
            "other",
            UserModelKind::Preference,
            "Be concise.",
            "style",
            &evidence,
            "gw_voice",
            1,
        )
        .unwrap()
        .unwrap();
    store
        .review_candidate(
            &candidate.id,
            ReviewAction::Accept,
            "operator",
            None,
            None,
            2,
        )
        .unwrap();
    let correction = store
        .active_heads(Some(2))
        .unwrap()
        .into_iter()
        .find(|r| r.source_candidate.is_some())
        .unwrap();
    store
        .set_voice_consent(&correction.id, "moss-live", VoiceConsentAction::Grant, 0, 3)
        .unwrap();
    let exported = store
        .voice_export_heads(&context(), "moss-live", 3)
        .unwrap();
    assert_eq!(
        exported
            .iter()
            .map(|r| r.semantic_key.as_str())
            .collect::<Vec<_>>(),
        ["channel", "global", "main", "session"]
    );
    assert!(
        store
            .voice_export_heads(
                &ApplicabilityContext::new("other", "wss", "gw_voice"),
                "moss-live",
                3
            )
            .unwrap()
            .contains(&correction)
    );
}

#[test]
fn additive_migration_preserves_legacy_heads_without_granting_consent() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserModelStore::open(dir.path()).unwrap();
    let head = fact(&store, "legacy", "agent:main", 1);
    store
        .conn
        .lock()
        .execute_batch("DROP TABLE user_model_voice_consent_receipts;")
        .unwrap();
    drop(store);
    let store = UserModelStore::open(dir.path()).unwrap();
    assert_eq!(
        store.active_heads(Some(2)).unwrap(),
        std::slice::from_ref(&head)
    );
    assert!(
        store
            .voice_export_heads(&context(), "moss-live", 2)
            .unwrap()
            .is_empty()
    );
    store
        .set_voice_consent(&head.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
        .unwrap();
    drop(store);
    let store = UserModelStore::open(dir.path()).unwrap();
    assert_eq!(
        store
            .voice_export_heads(&context(), "moss-live", 3)
            .unwrap(),
        [head]
    );
}

#[test]
fn invalid_provider_and_failed_receipt_write_leave_no_authority() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserModelStore::open(dir.path()).unwrap();
    let head = fact(&store, "style", "global", 1);
    for provider in [
        "",
        " moss-live",
        "MOSS",
        "moss/live",
        "moss\nlive",
        &"a".repeat(65),
    ] {
        assert!(
            store
                .set_voice_consent(&head.id, provider, VoiceConsentAction::Grant, 0, 2)
                .is_err()
        );
    }
    store.conn.lock().execute_batch("CREATE TRIGGER fail_voice_receipt BEFORE INSERT ON user_model_voice_consent_receipts BEGIN SELECT RAISE(ABORT, 'synthetic write failure'); END;").unwrap();
    assert!(
        store
            .set_voice_consent(&head.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
            .is_err()
    );
    assert_eq!(store.voice_consent(&head.id, "moss-live").unwrap(), None);
    assert_eq!(store.active_heads(Some(2)).unwrap(), [head]);
    assert!(
        store
            .voice_export_heads(&context(), "moss-live", 2)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn concurrent_consent_editors_cannot_overwrite_a_committed_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let store = UserModelStore::open(dir.path()).unwrap();
    let head = fact(&store, "style", "global", 1);
    store
        .set_voice_consent(&head.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
        .unwrap();
    let stale = UserModelStore::open(dir.path()).unwrap();
    let shown = stale.voice_consent(&head.id, "moss-live").unwrap().unwrap();
    store
        .set_voice_consent(&head.id, "moss-live", VoiceConsentAction::Revoke, 1, 3)
        .unwrap();
    let error = stale
        .set_voice_consent(
            &head.id,
            "moss-live",
            VoiceConsentAction::Grant,
            shown.revision,
            4,
        )
        .unwrap_err();
    assert!(is_user_model_head_conflict(&error));
    assert!(
        stale
            .voice_export_heads(&context(), "moss-live", 4)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        stale
            .voice_consent(&head.id, "moss-live")
            .unwrap()
            .unwrap()
            .revision,
        2
    );
}
