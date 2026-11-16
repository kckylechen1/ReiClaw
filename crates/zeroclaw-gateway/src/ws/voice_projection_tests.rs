use super::*;
use zeroclaw_memory::companion::*;
use zeroclaw_runtime::agent::persona_projection::*;

// Inspect actual text bytes, not parsed-and-reserialized JSON.
async fn raw_text(socket: &mut DeviceWire) -> String {
    tokio::time::timeout(LIMIT, async {
        assert_eq!(socket.0.read_u8().await.unwrap(), 0x81);
        let second = socket.0.read_u8().await.unwrap();
        assert_eq!(second & 0x80, 0);
        let len = match second {
            126 => usize::from(socket.0.read_u16().await.unwrap()),
            127 => usize::try_from(socket.0.read_u64().await.unwrap()).unwrap(),
            len => usize::from(len),
        };
        assert!(len < 4096);
        let mut bytes = vec![0; len];
        socket.0.read_exact(&mut bytes).await.unwrap();
        String::from_utf8(bytes).unwrap()
    })
    .await
    .unwrap()
}

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
async fn voice_projection_refuses_non_device_credentials_before_upgrade() {
    for scenario in [
        "paired",
        "anonymous",
        "legacy-bridge",
        "owner-bridge",
        "wrong-agent",
        "no-chat",
    ] {
        let fixture = DeviceFixture::with_pairing(&["chat"], scenario != "anonymous").await;
        owner_soul(&fixture);
        let config = fixture.state.config.read().clone();
        let store = UserModelStore::shared(&config.data_dir).unwrap();
        let head = store
            .record_owner_statement(
                UserModelKind::Preference,
                "PRIVATE_CONSENTED_FACT",
                "style",
                "global",
                1,
            )
            .unwrap();
        store
            .set_voice_consent(&head.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
            .unwrap();
        let token = match scenario {
            "paired" => OWNER_TOKEN,
            "anonymous" => {
                fixture.state.config.write().gateway.bridges.clear();
                ""
            }
            _ => {
                let mut config = fixture.state.config.write();
                let bridge = config.gateway.bridges.get_mut("device").unwrap();
                match scenario {
                    "legacy-bridge" => bridge.device = None,
                    "owner-bridge" => {
                        bridge.device = None;
                        bridge.owner = Some(zeroclaw_config::schema::GatewayOwnerBridgeConfig {
                            agent: "device-agent".into(),
                            operations: vec!["chat".into()],
                        });
                    }
                    "wrong-agent" => bridge.device.as_mut().unwrap().agent = "other-agent".into(),
                    "no-chat" => bridge.device.as_mut().unwrap().operations = vec!["cancel".into()],
                    _ => unreachable!(),
                }
                DEVICE_TOKEN
            }
        };
        let (status, mut socket) = DeviceWire::upgrade(
            fixture.address,
            &format!(
                "agent=device-agent&session_id={SESSION}&surface=voice&voice_provider=moss-live"
            ),
            token,
        )
        .await;
        assert_eq!(status, 403, "{scenario}");
        let response = http_error(&mut socket).await;
        let expected = if matches!(scenario, "wrong-agent" | "no-chat") {
            "BRIDGE_BINDING_REJECTED"
        } else {
            "VOICE_DEVICE_CREDENTIAL_REQUIRED"
        };
        assert_eq!(response["code"], expected, "{scenario}: {response}");
        assert!(response.get("voice_projection").is_none());
        assert!(!response.to_string().contains("PRIVATE_CONSENTED_FACT"));
        assert!(fixture.seen.lock().is_empty());
    }
}

#[tokio::test]
async fn voice_projection_disabled_alias_is_refused_before_build() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let config = {
        let mut config = fixture.state.config.write();
        config.agents.get_mut("device-agent").unwrap().enabled = false;
        config.clone()
    };
    // An unreadable store would fail if projection construction were attempted.
    std::fs::create_dir(config.data_dir.join(SOUL_PROFILE_DB_FILE)).unwrap();
    let (status, mut socket) = DeviceWire::upgrade(
        fixture.address,
        &format!("agent=device-agent&session_id={SESSION}&surface=voice&voice_provider=moss-live"),
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 403);
    let response = http_error(&mut socket).await;
    assert_eq!(response["code"], "VOICE_ALIAS_DISABLED");
    assert!(response.get("voice_projection").is_none());
    assert!(!config.data_dir.join("user_model.db").exists());
    assert!(fixture.seen.lock().is_empty());
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
    let ordinary = persona_projection(&config, "device-agent").section.unwrap();
    // Split the assembled persona, preserving its exact section bytes. Renderer
    // output alone misses persona_projection's trim_end/join boundary.
    for (field, heading, next) in [
        ("identity", "## Identity", "## Principles"),
        ("principles", "## Principles", "## Tone"),
        ("tone", "## Tone", TONE_EXEMPLARS_FRAMING_LINE),
    ] {
        let section = &ordinary[ordinary.find(heading).unwrap()..];
        let expected = section.split_once(&format!("\n\n{next}")).unwrap().0;
        assert_eq!(
            projection[field].as_str().unwrap().as_bytes(),
            expected.as_bytes(),
            "{field}"
        );
    }
    assert!(
        ordinary.contains("PRIVATE_SELF_STORY")
            && ordinary.contains("PRIVATE_BOND_STORY")
            && ordinary.contains("PRIVATE_EXEMPLAR")
    );
    assert_eq!(
        projection["owner_facts"],
        project_active_heads(
            std::slice::from_ref(&allowed),
            USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS
        )
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
        let start = raw_text(&mut socket).await;
        assert_eq!(
            start,
            json!({"type":"session_start", "session_id":SESSION, "resumed":false,
            "message_count":0, "surface_version":1, "surface":surface})
            .to_string()
        );
        socket
            .send(json!({"type":"connect", "surface":surface}))
            .await;
        assert_eq!(
            raw_text(&mut socket).await,
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
        raw_text(&mut late).await,
        json!({"type":"session_start", "session_id":SESSION,
        "resumed":false, "message_count":0, "surface_version":1, "surface":null})
        .to_string()
    );
    late.send(json!({"type":"connect", "surface":"voice"}))
        .await;
    assert_eq!(late.next().await.unwrap()["code"], "VOICE_QUERY_REQUIRED");
    assert!(fixture.seen.lock().is_empty());
}

struct ReducingMetadata {
    inner: Arc<dyn zeroclaw_infra::session_backend::SessionBackend>,
    reduce: Box<dyn Fn() + Send + Sync>,
    calls: std::sync::atomic::AtomicUsize,
}

impl zeroclaw_infra::session_backend::SessionBackend for ReducingMetadata {
    fn load(&self, key: &str) -> Vec<zeroclaw_providers::ChatMessage> {
        self.inner.load(key)
    }
    fn append(&self, key: &str, message: &zeroclaw_providers::ChatMessage) -> std::io::Result<()> {
        self.inner.append(key, message)
    }
    fn remove_last(&self, key: &str) -> std::io::Result<bool> {
        self.inner.remove_last(key)
    }
    fn list_sessions(&self) -> Vec<String> {
        self.inner.list_sessions()
    }
    fn get_session_agent_alias(&self, key: &str) -> std::io::Result<Option<String>> {
        self.inner.get_session_agent_alias(key)
    }
    fn get_session_metadata(
        &self,
        key: &str,
    ) -> Option<zeroclaw_infra::session_backend::SessionMetadata> {
        let metadata = self.inner.get_session_metadata(key);
        // Mutate only this fixture, at the actual metadata boundary after the
        // initial binding check. No worker needs to wait for the test driver.
        assert_eq!(self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst), 0);
        (self.reduce)();
        metadata
    }
}

#[tokio::test]
async fn voice_projection_rechecks_live_admission_after_metadata() {
    for reduction in [
        "bridge",
        "chat",
        "alias",
        "credential",
        "device-scope",
        "device-agent",
    ] {
        let mut fixture = DeviceFixture::new(&["chat"]).await;
        owner_soul(&fixture);
        let config = fixture.state.config.read().clone();
        let facts = UserModelStore::shared(&config.data_dir).unwrap();
        let head = facts
            .record_owner_statement(
                UserModelKind::Preference,
                "PRIVATE_CONSENTED_FACT",
                "style",
                "global",
                1,
            )
            .unwrap();
        facts
            .set_voice_consent(&head.id, "moss-live", VoiceConsentAction::Grant, 0, 2)
            .unwrap();
        let live_config = fixture.state.config.clone();
        let backend = Arc::new(ReducingMetadata {
            inner: fixture.state.session_backend.as_ref().unwrap().clone(),
            reduce: Box::new(move || {
                let mut config = live_config.write();
                match reduction {
                    "bridge" => config.gateway.bridges.clear(),
                    "chat" => config
                        .gateway
                        .bridges
                        .get_mut("device")
                        .unwrap()
                        .device
                        .as_mut()
                        .unwrap()
                        .operations
                        .clear(),
                    "alias" => config.agents.get_mut("device-agent").unwrap().enabled = false,
                    "credential" => {
                        config.gateway.bridges.get_mut("device").unwrap().token_hash =
                            zeroclaw_config::tokens::hash_token("rotated-device-token");
                    }
                    "device-scope" => {
                        config.gateway.bridges.get_mut("device").unwrap().device = None;
                    }
                    "device-agent" => {
                        config
                            .gateway
                            .bridges
                            .get_mut("device")
                            .unwrap()
                            .device
                            .as_mut()
                            .unwrap()
                            .agent = "other-agent".into();
                    }
                    _ => unreachable!(),
                }
            }),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut state = fixture.state.clone();
        state.session_backend = Some(backend.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/ws/chat", axum::routing::get(handle_ws_chat))
            .with_state(state);
        fixture.tasks.push(zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let (status, mut socket) = DeviceWire::upgrade(
            address,
            &format!(
                "agent=device-agent&session_id={SESSION}&surface=voice&voice_provider=moss-live"
            ),
            DEVICE_TOKEN,
        )
        .await;
        assert_eq!(status, 101);
        let first = socket.next().await.unwrap();
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(first["type"], "error", "{reduction}: {first}");
        assert_eq!(
            first["code"],
            if reduction == "alias" {
                "VOICE_ALIAS_DISABLED"
            } else {
                "VOICE_AUTHORIZATION_CHANGED"
            },
            "{reduction}: {first}"
        );
        assert!(first.get("voice_projection").is_none());
        assert!(!first.to_string().contains("PRIVATE_CONSENTED_FACT"));
        assert!(socket.next().await.is_none());
        assert!(fixture.seen.lock().is_empty());
    }
}

#[tokio::test]
async fn voice_refuses_missing_and_invalid_device_profiles_at_upgrade() {
    for invalid in ["missing", "unknown", "full", "unbounded", "prompt", "mcp"] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        {
            let mut config = fixture.state.config.write();
            match invalid {
                "missing" => config.gateway.bridges.get_mut("device").unwrap().device.as_mut().unwrap().risk_profile = None,
                "unknown" => config.gateway.bridges.get_mut("device").unwrap().device.as_mut().unwrap().risk_profile = Some("absent".into()),
                "full" => config.risk_profiles.get_mut("public-device").unwrap().level = zeroclaw_config::autonomy::AutonomyLevel::Full,
                "unbounded" => config.risk_profiles.get_mut("public-device").unwrap().allowed_tools = None,
                "prompt" => config.risk_profiles.get_mut("public-device").unwrap().always_ask.push("*".into()),
                "mcp" => config.risk_profiles.get_mut("public-device").unwrap().mcp_discovered_tool_policy = zeroclaw_config::autonomy::McpDiscoveredToolPolicy::AutoAdmit,
                _ => unreachable!(),
            }
        }
        let (status, mut socket) = DeviceWire::upgrade(fixture.address,
            &format!("agent=device-agent&session_id={SESSION}&surface=voice&voice_provider=test"), DEVICE_TOKEN).await;
        assert_eq!(status, 403, "{invalid}");
        assert_eq!(http_error(&mut socket).await["code"], "VOICE_DEVICE_PROFILE_INVALID");
        assert!(fixture.seen.lock().is_empty());
    }
}
