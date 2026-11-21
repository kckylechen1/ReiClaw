use super::*;
use reiclaw_api::delegation_request::{
    DELEGATION_ORIGIN_CONTEXT, DelegationOrigin, DelegationOriginResolver, DelegationOriginScope,
};
use reiclaw_api::memory_traits::MemoryReadPolicy;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-delegation-origin-owner";

struct OriginFixture {
    _tmp: tempfile::TempDir,
    state: AppState,
    scope: WsTurnScope,
    identity: reiclaw_api::companion::AgentIdentityId,
    invalidated: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl OriginFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = crate::tests::admin_paircode_state(&tmp, true, false);
        state.pairing = Arc::new(reiclaw_runtime::security::pairing::PairingGuard::new(
            true,
            &[TOKEN.into()],
        ));
        let identity = {
            let mut config = state.config.write();
            config.agents.insert("web".into(), Default::default());
            reiclaw_memory::companion::resolve_or_mint_agent_identity(
                &config.companion_memory.resolved_store_dir(&config.data_dir),
                "web",
            )
            .unwrap()
        };
        Self {
            _tmp: tmp,
            state,
            scope: WsTurnScope {
                session_key: "gw_source-a".into(),
                session_id: "source-a".into(),
                auth_subject: Some(reiclaw_config::tokens::hash_token(TOKEN)),
                surface: None,
                bridge: None,
            },
            identity,
            invalidated: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        }
    }

    fn resolver(&self, owner_input: bool) -> DelegationOriginResolver {
        delegation_origin_resolver(
            &self.state,
            &self.scope,
            "web",
            Some(&self.identity),
            owner_input,
            Some(Arc::clone(&self.invalidated)),
            self.cancel.clone(),
        )
    }
}

async fn resolve(
    resolver: &DelegationOriginResolver,
    alias: &str,
    session: &str,
    memory: &[&str],
    policy: MemoryReadPolicy,
) -> Option<DelegationOrigin> {
    reiclaw_api::TOOL_LOOP_SESSION_KEY
        .scope(
            Some(session.into()),
            reiclaw_api::TOOL_LOOP_MEMORY_SESSIONS.scope(
                memory.iter().map(|s| (*s).to_string()).collect(),
                reiclaw_api::TOOL_LOOP_MEMORY_READ_POLICY.scope(policy, async { resolver(alias) }),
            ),
        )
        .await
}

async fn source(
    fixture: &OriginFixture,
    resolver: &DelegationOriginResolver,
) -> Option<DelegationOrigin> {
    resolve(
        resolver,
        "web",
        &fixture.scope.session_key,
        &["source-a"],
        MemoryReadPolicy::SessionScoped,
    )
    .await
}

#[tokio::test]
async fn ws_delegation_source_uses_exact_host_session_and_memory_scope() {
    let fixture = OriginFixture::new();
    let resolver = fixture.resolver(true);
    assert_eq!(
        source(&fixture, &resolver).await,
        Some(DelegationOrigin {
            session_key: "gw_source-a".into(),
            scope: DelegationOriginScope::SessionFacts {
                operator_subject: reiclaw_config::tokens::hash_token(TOKEN),
                agent_id: fixture.identity.as_str().into(),
                memory_session_id: None,
            },
        })
    );
    assert!(
        resolver("web").is_none(),
        "no ambient host turn is admitted"
    );
    for (alias, session) in [
        ("other-agent", "gw_source-a"),
        ("web", "gw_source-b"),
        ("web", "agent:web"),
    ] {
        assert!(
            resolve(
                &resolver,
                alias,
                session,
                &["source-a"],
                MemoryReadPolicy::SessionScoped
            )
            .await
            .is_none()
        );
    }
    for memory in [vec!["source-b"], vec!["source-a", "source-b"], vec![]] {
        let origin = resolve(
            &resolver,
            "web",
            "gw_source-a",
            &memory,
            MemoryReadPolicy::SessionScoped,
        )
        .await
        .unwrap();
        assert_eq!(origin.scope, DelegationOriginScope::SourceOnly);
    }
    assert_eq!(
        source(&fixture, &fixture.resolver(false))
            .await
            .unwrap()
            .scope,
        DelegationOriginScope::SourceOnly
    );
    fixture.invalidated.store(true, Ordering::Release);
    assert_eq!(
        source(&fixture, &resolver).await.unwrap().scope,
        DelegationOriginScope::SourceOnly
    );
    fixture.cancel.cancel();
    assert!(source(&fixture, &resolver).await.is_none());
}

#[tokio::test]
async fn colliding_ws_memory_ids_never_grant_ordinary_memory_reads() {
    for raw in ["source.a", "source_a"] {
        let mut fixture = OriginFixture::new();
        fixture.scope.session_id = raw.into();
        fixture.scope.session_key = format!("gw_{raw}");
        let resolver = fixture.resolver(true);
        let origin = resolve(
            &resolver,
            "web",
            &fixture.scope.session_key,
            &["source_a"],
            MemoryReadPolicy::SessionScoped,
        )
        .await
        .unwrap();
        assert_eq!(origin.session_key, format!("gw_{raw}"));
        assert!(matches!(
            origin.scope,
            DelegationOriginScope::SessionFacts {
                memory_session_id: None,
                ..
            }
        ));
    }
}

#[tokio::test]
async fn ws_delegation_source_rechecks_pairing_agent_identity_and_roots() {
    for change in ["token", "agent", "identity", "root", "memory-root"] {
        let fixture = OriginFixture::new();
        let resolver = fixture.resolver(true);
        assert!(source(&fixture, &resolver).await.is_some());
        match change {
            "token" => assert!(fixture.state.pairing.revoke_token(TOKEN)),
            "agent" => {
                fixture
                    .state
                    .config
                    .write()
                    .agents
                    .get_mut("web")
                    .unwrap()
                    .enabled = false
            }
            "identity" => {
                let config = fixture.state.config.read();
                let store = config.companion_memory.resolved_store_dir(&config.data_dir);
                reiclaw_memory::companion::invalidate_alias_identity(&store, "web").unwrap();
                let replacement =
                    reiclaw_memory::companion::resolve_or_mint_agent_identity(&store, "web")
                        .unwrap();
                assert_ne!(replacement, fixture.identity);
            }
            "root" => {
                fixture.state.config.write().data_dir = fixture._tmp.path().join("other-data")
            }
            "memory-root" => {
                fixture.state.config.write().companion_memory.store_dir =
                    Some(fixture._tmp.path().join("other-memory"))
            }
            _ => unreachable!(),
        }
        assert!(source(&fixture, &resolver).await.is_none(), "{change}");
    }
}

#[tokio::test]
async fn ws_delegation_device_source_has_no_fact_grant_and_honors_revocation() {
    let mut fixture = OriginFixture::new();
    let binding = {
        let mut config = fixture.state.config.write();
        config.risk_profiles.insert(
            "public-source".into(),
            reiclaw_config::schema::RiskProfileConfig {
                level: reiclaw_config::autonomy::AutonomyLevel::Supervised,
                allowed_tools: Some(vec![]),
                auto_approve: vec![],
                always_ask: vec![],
                mcp_discovered_tool_policy:
                    reiclaw_config::autonomy::McpDiscoveredToolPolicy::ExplicitOnly,
                ..Default::default()
            },
        );
        let bridge = reiclaw_config::schema::GatewayBridgeConfig {
            token_hash: reiclaw_config::tokens::hash_token("synthetic-device-origin"),
            sessions: vec!["source-a".into()],
            device: Some(reiclaw_config::schema::GatewayDeviceBridgeConfig {
                agent: "web".into(),
                risk_profile: Some("public-source".into()),
                operations: vec!["chat".into()],
            }),
            ..Default::default()
        };
        config
            .gateway
            .bridges
            .insert("source-device".into(), bridge.clone());
        device::Binding::new("source-device".into(), bridge, &config)
    };
    fixture.scope.auth_subject = Some(binding.initial.token_hash.clone());
    fixture.scope.bridge = Some(binding);
    let resolver = fixture.resolver(true);
    let admitted = source(&fixture, &resolver).await.unwrap();
    assert_eq!(admitted.session_key, "gw_source-a");
    assert_eq!(admitted.scope, DelegationOriginScope::SourceOnly);
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .get_mut("source-device")
        .unwrap()
        .sessions = vec!["source-b".into()];
    assert!(source(&fixture, &resolver).await.is_none());
}

struct OriginProbe(AtomicU8);

impl reiclaw_api::attribution::Attributable for OriginProbe {
    fn role(&self) -> reiclaw_api::attribution::Role {
        reiclaw_api::attribution::Role::Provider(reiclaw_api::attribution::ProviderKind::Model(
            reiclaw_api::attribution::ModelProviderKind::Custom,
        ))
    }
    fn alias(&self) -> &str {
        "origin-probe"
    }
}

#[async_trait::async_trait]
impl reiclaw_api::model_provider::ModelProvider for OriginProbe {
    async fn chat_with_system(
        &self,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<String> {
        self.0.store(
            match DELEGATION_ORIGIN_CONTEXT.try_with(|resolver| resolver("web")) {
                Err(_) => 1,
                Ok(None) => 2,
                Ok(Some(_)) => 3,
            },
            Ordering::SeqCst,
        );
        Ok("Origin probe complete".into())
    }
}

#[tokio::test]
async fn real_tool_loop_denies_inherited_delegation_origin_in_nested_and_scheduled_turns() {
    use reiclaw_api::ingress::{IngressContext, TurnOrigin};
    use reiclaw_runtime::agent::loop_::{
        LoopKnobs, ResolvedAgentExecution, ResolvedModelAccess, ToolLoop, run_tool_call_loop,
    };

    for inherited in [false, true] {
        for origin in [
            TurnOrigin::Interactive,
            TurnOrigin::Channel,
            TurnOrigin::AgentDirect,
            TurnOrigin::SubTurn,
            TurnOrigin::Cron,
            TurnOrigin::Daemon,
        ] {
            let provider = OriginProbe(AtomicU8::new(0));
            let mut history = vec![reiclaw_providers::ChatMessage::user("Probe source context")];
            let multimodal = Default::default();
            let pacing = Default::default();
            let knobs = LoopKnobs::default();
            let turn = ToolLoop::for_turn(
                ResolvedAgentExecution {
                    task_owner: None,
                    model_access: ResolvedModelAccess {
                        model_provider: &provider,
                        provider_name: "origin-probe",
                        model: "test",
                        temperature: None,
                    },
                    tools_registry: &[],
                    observer: &reiclaw_runtime::observability::NoopObserver,
                    silent: true,
                    approval: None,
                    multimodal_config: &multimodal,
                    config: None,
                    max_tool_iterations: 1,
                    hooks: None,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    activated_tools: None,
                    pacing: &pacing,
                    strict_tool_parsing: true,
                    parallel_tools: false,
                    max_tool_result_chars: 0,
                    keep_tool_context_turns: 2,
                    context_token_budget: 0,
                    receipt_generator: None,
                    knobs: &knobs,
                },
                &mut history,
                reiclaw_runtime::agent::TurnMeta {
                    agent_alias: Some("web"),
                    parent_agent_alias: None,
                    turn_id: "origin-test",
                    channel_name: "wss",
                },
                IngressContext::from_origin(origin),
            );
            if inherited {
                let resolver: DelegationOriginResolver = Arc::new(|_| {
                    Some(DelegationOrigin {
                        session_key: "gw_source-a".into(),
                        scope: DelegationOriginScope::SourceOnly,
                    })
                });
                DELEGATION_ORIGIN_CONTEXT
                    .scope(resolver, run_tool_call_loop(turn))
                    .await
                    .unwrap();
            } else {
                run_tool_call_loop(turn).await.unwrap();
            }
            let expected = if !inherited {
                1
            } else if matches!(
                origin,
                TurnOrigin::SubTurn | TurnOrigin::Cron | TurnOrigin::Daemon
            ) {
                2
            } else {
                3
            };
            assert_eq!(
                provider.0.load(Ordering::SeqCst),
                expected,
                "{origin:?}, inherited={inherited}"
            );
            assert!(DELEGATION_ORIGIN_CONTEXT.try_with(|_| ()).is_err());
        }
    }
}
