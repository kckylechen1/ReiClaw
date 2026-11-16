use super::*;
use zeroclaw_memory::companion::*;
use zeroclaw_runtime::agent::persona_projection::*;

fn owner_soul(fixture: &DeviceFixture) -> Arc<SoulProfileStore> {
    let dir = fixture.state.config.read().data_dir.clone();
    let soul = SoulProfileStore::shared(&dir).unwrap();
    soul.set_identity(
        "device-agent",
        SoulIdentity {
            name: "Voice Test Agent".into(),
            self_description: Some("A synthetic test persona.".into()),
            primary_language: Some("zh".into()),
            pronouns: Some("they/them".into()),
        },
        0,
        1,
    )
    .unwrap();
    soul.set_principles(
        "device-agent",
        SoulPrinciples {
            items: vec!["Keep private things private.".into()],
        },
        0,
        1,
    )
    .unwrap();
    soul
}

async fn voice_socket(fixture: &DeviceFixture, provider: &str) -> DeviceWire {
    let (status, socket) = DeviceWire::upgrade(
        fixture.address,
        &format!("agent=device-agent&session_id={SESSION}&surface=voice&voice_provider={provider}"),
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    socket
}

async fn http_error(socket: &mut DeviceWire) -> Value {
    tokio::time::timeout(LIMIT, async {
        let mut body = Vec::new();
        loop {
            assert!(body.len() < 4096);
            body.push(socket.0.read_u8().await.unwrap());
            if let Ok(value) = serde_json::from_slice(&body) {
                return value;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn voice_projection_real_handshake_equals_persona_bytes_and_excludes_private_layers() {
    let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
    let soul = owner_soul(&fixture);
    soul.set_tone(
        "device-agent",
        SoulTone {
            heads: [(
                "warmth".into(),
                zeroclaw_config::persona::PersonaLevel::High,
            )]
            .into(),
        },
        0,
        2,
    )
    .unwrap();
    soul.set_story(
        "device-agent",
        SoulStory {
            entries: vec![
                StoryEntry {
                    kind: StoryKind::SelfView,
                    text: "PRIVATE_SELF_STORY".into(),
                },
                StoryEntry {
                    kind: StoryKind::Bond,
                    text: "PRIVATE_BOND_STORY".into(),
                },
            ],
        },
        0,
        2,
    )
    .unwrap();
    soul.set_tone_exemplars(
        "device-agent",
        SoulToneExemplars {
            exemplars: vec![ToneExemplar {
                situation: "PRIVATE_SITUATION".into(),
                owner_line: "PRIVATE_OWNER_LINE".into(),
                agent_line: "PRIVATE_EXEMPLAR".into(),
            }],
        },
        0,
        2,
    )
    .unwrap();
    let config = fixture.state.config.read().clone();
    let store = UserModelStore::shared(&config.data_dir).unwrap();
    let allowed = store
        .record_owner_statement(
            UserModelKind::Preference,
            "Prefers concise replies.",
            "style",
            "agent:device-agent",
            1,
        )
        .unwrap();
    store
        .record_owner_statement(
            UserModelKind::Preference,
            "PRIVATE_UNCONSENTED",
            "private",
            "global",
            1,
        )
        .unwrap();
    store
        .record_observation(
            UserModelKind::Preference,
            "PRIVATE_CANDIDATE",
            "candidate",
            "[]",
            1,
        )
        .unwrap();
    store
        .set_voice_consent(&allowed.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
        .unwrap();

    let mut socket = voice_socket(&fixture, "moss-live").await;
    let start = socket.next().await.unwrap();
    assert_eq!(start["type"], "session_start");
    assert_eq!(start["surface"], "voice");
    let projection = &start["voice_projection"];
    assert_eq!(
        projection
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "identity",
            "owner_facts",
            "principles",
            "provider",
            "tone",
            "version"
        ]
    );
    assert_eq!(projection["version"], 1);
    assert_eq!(projection["provider"], "moss-live");
    let profile = soul.profile("device-agent").unwrap();
    assert_eq!(projection["identity"], render_identity(&profile).unwrap());
    assert!(
        projection["identity"]
            .as_str()
            .unwrap()
            .contains("Pronouns: they/them.")
    );
    assert!(
        projection["identity"]
            .as_str()
            .unwrap()
            .contains(IDENTITY_HONESTY_LINE)
    );
    assert_eq!(
        projection["principles"],
        render_principles(&profile).unwrap()
    );
    assert_eq!(
        projection["tone"],
        resolve_effective_tone(&config, "device-agent", Some(&profile))
            .0
            .to_prompt_section()
            .unwrap()
    );
    let ordinary = persona_projection(&config, "device-agent").section.unwrap();
    for field in ["identity", "principles", "tone"] {
        assert!(ordinary.contains(projection[field].as_str().unwrap()));
    }
    assert!(
        ordinary.contains("PRIVATE_SELF_STORY")
            && ordinary.contains("PRIVATE_BOND_STORY")
            && ordinary.contains("PRIVATE_EXEMPLAR")
    );
    assert_eq!(
        projection["owner_facts"],
        project_active_heads(&[allowed.clone()], USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS)
            .prompt_section
    );
    assert!(!projection.to_string().contains("PRIVATE_"));
    socket
        .send(json!({"type":"connect", "surface":"voice"}))
        .await;
    assert!(
        socket
            .until("connected")
            .await
            .get("voice_projection")
            .is_none()
    );
    // Even a paired-token collision remains unverified voice input.
    socket
        .send(json!({"type":"message", "content":"A synthetic voice question."}))
        .await;
    socket.until("done").await;
    let rows = fixture
        .state
        .session_backend
        .as_ref()
        .unwrap()
        .load_with_timestamps(&format!("gw_{SESSION}"));
    let users: Vec<_> = rows
        .iter()
        .filter(|row| row.message.role == "user")
        .collect();
    assert_eq!(users.len(), 1);
    assert!(users[0].ingress.is_none());

    let mut other = voice_socket(&fixture, "openai-realtime").await;
    assert_eq!(
        other.next().await.unwrap()["voice_projection"]["owner_facts"],
        ""
    );
    store
        .set_voice_consent(&allowed.id, "moss-live", VoiceConsentAction::Revoke, 1, 3)
        .unwrap();
    let mut revoked = voice_socket(&fixture, "moss-live").await;
    assert_eq!(
        revoked.next().await.unwrap()["voice_projection"]["owner_facts"],
        ""
    );
}

#[tokio::test]
async fn voice_projection_rejects_each_unavailable_identity_before_session_start() {
    for scenario in [
        "missing",
        "legacy",
        "seed",
        "unreadable",
        "degraded",
        "user-model",
    ] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        let config = fixture.state.config.read().clone();
        let expected = match scenario {
            "missing" => "VOICE_OWNER_IDENTITY_REQUIRED",
            "legacy" => {
                std::fs::write(fixture.workspace.join("SOUL.md"), "PRIVATE_LEGACY_SOUL").unwrap();
                std::fs::write(
                    fixture.workspace.join("IDENTITY.md"),
                    "PRIVATE_LEGACY_IDENTITY",
                )
                .unwrap();
                "VOICE_OWNER_IDENTITY_REQUIRED"
            }
            "seed" => {
                SoulProfileStore::open(&config.data_dir)
                    .unwrap()
                    .ensure_seeded("device-agent", "Seed", 1)
                    .unwrap();
                "VOICE_OWNER_IDENTITY_REQUIRED"
            }
            "unreadable" => {
                std::fs::create_dir(config.data_dir.join(SOUL_PROFILE_DB_FILE)).unwrap();
                "VOICE_SOUL_UNAVAILABLE"
            }
            "degraded" => {
                std::fs::write(
                    config.data_dir.join(SOUL_PROFILE_DB_FILE),
                    "not a sqlite database",
                )
                .unwrap();
                assert!(
                    persona_projection(&config, "device-agent")
                        .section
                        .unwrap()
                        .contains(IDENTITY_UNAVAILABLE_LINE)
                );
                "VOICE_SOUL_UNAVAILABLE"
            }
            "user-model" => {
                owner_soul(&fixture);
                std::fs::create_dir(config.data_dir.join("user_model.db")).unwrap();
                "VOICE_USER_MODEL_UNAVAILABLE"
            }
            _ => unreachable!(),
        };
        let mut socket = voice_socket(&fixture, "moss-live").await;
        let first = socket.next().await.unwrap();
        assert_eq!(first["type"], "error", "{scenario}: {first}");
        assert_eq!(first["code"], expected, "{scenario}");
        assert!(first.get("voice_projection").is_none());
        assert!(socket.next().await.is_none());
        assert!(fixture.seen.lock().is_empty());
    }
}

#[tokio::test]
async fn voice_projection_requires_provider_alias_and_query_surface_without_changing_nonvoice_frames()
 {
    let fixture = DeviceFixture::new(&["chat"]).await;
    for (query, code) in [
        (
            format!("agent=device-agent&session_id={SESSION}&surface=voice"),
            "VOICE_PROVIDER_REQUIRED",
        ),
        (
            format!("agent=device-agent&session_id={SESSION}&surface=voice&voice_provider=MOSS"),
            "VOICE_PROVIDER_INVALID",
        ),
        (
            format!("agent=unknown&session_id={SESSION}&surface=voice&voice_provider=moss-live"),
            "VOICE_UNKNOWN_ALIAS",
        ),
    ] {
        let (status, mut socket) = DeviceWire::upgrade(fixture.address, &query, DEVICE_TOKEN).await;
        assert_eq!(status, 400);
        assert_eq!(http_error(&mut socket).await["code"], code);
    }
    for surface in ["web", "cli", "telegram"] {
        let (status, mut socket) = DeviceWire::upgrade(
            fixture.address,
            &format!(
                "agent=device-agent&session_id={SESSION}&surface={surface}&voice_provider=MOSS"
            ),
            DEVICE_TOKEN,
        )
        .await;
        assert_eq!(status, 101);
        let start = socket.next().await.unwrap();
        assert_eq!(
            start.to_string(),
            json!({"type":"session_start", "session_id":SESSION, "resumed":false,
            "message_count":0, "surface_version":1, "surface":surface})
            .to_string()
        );
        socket
            .send(json!({"type":"connect", "surface":surface}))
            .await;
        assert_eq!(
            socket.next().await.unwrap().to_string(),
            json!({"type":"connected",
            "owner_bridge_cancel":false, "owner_bridge_approval_once":false,
            "message":"Connection established", "surface_version":1, "surface":surface})
            .to_string()
        );
    }
    let (status, mut late) = DeviceWire::upgrade(
        fixture.address,
        &format!("agent=device-agent&session_id={SESSION}&voice_provider=moss-live"),
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    assert_eq!(
        late.next().await.unwrap().to_string(),
        json!({"type":"session_start", "session_id":SESSION,
        "resumed":false, "message_count":0, "surface_version":1, "surface":null})
        .to_string()
    );
    late.send(json!({"type":"connect", "surface":"voice"}))
        .await;
    assert_eq!(late.next().await.unwrap()["code"], "VOICE_QUERY_REQUIRED");
    assert!(fixture.seen.lock().is_empty());
}
