//! Channel/gateway `process_message` entry point for the agent loop.
//!
//! Extracted from `loop_/mod.rs` so the interactive CLI `run` path and the
//! inbound message path can evolve independently.

use crate::approval::ApprovalManager;
use crate::observability::{self, Observer};
use crate::platform;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools;
use crate::tools::scoped;
use anyhow::{Context, Result};
use reiclaw_api::ingress::{IngressContext, TurnOrigin};
use reiclaw_config::schema::Config;
use reiclaw_memory::{self, Memory};
use reiclaw_providers::{ChatMessage, ModelProvider};
use std::sync::Arc;

use super::{
    LoopKnobs, ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess, ResolvedRuntimeKnobs,
    ToolLoop, build_system_prompt_for_turn, compute_excluded_mcp_tools, resolved_agent_for_turn,
};

/// Process a single message through the full agent (with tools and memory).
/// Used by channels (Telegram, etc.) to enable tool use.
pub async fn process_message(
    config: Config,
    agent_alias: &str,
    message: &str,
    session_id: Option<&str>,
    origin: TurnOrigin,
) -> Result<String> {
    process_message_with_task_identity(config, agent_alias, message, session_id, origin, None).await
}

pub async fn process_message_with_task_identity(
    config: Config,
    agent_alias: &str,
    message: &str,
    session_id: Option<&str>,
    origin: TurnOrigin,
    task_identity: Option<crate::tools::task_context::TaskIdentity>,
) -> Result<String> {
    process_message_with_hooks(
        config,
        agent_alias,
        message,
        session_id,
        origin,
        task_identity,
        None,
    )
    .await
}

pub async fn process_message_with_hooks(
    config: Config,
    agent_alias: &str,
    message: &str,
    session_id: Option<&str>,
    origin: TurnOrigin,
    task_identity: Option<crate::tools::task_context::TaskIdentity>,
    hooks: Option<Arc<crate::hooks::HookRunner>>,
) -> Result<String> {
    use ::reiclaw_log::Instrument;
    let agent = resolved_agent_for_turn(&config, agent_alias)?;
    crate::agent::thinking::validate_thinking_config(&agent.resolved.thinking);
    let risk_profile = config
        .risk_profile_for_agent(agent_alias)
        .with_context(|| {
            format!(
                "agents.{agent_alias}.risk_profile does not name a configured risk_profiles entry"
            )
        })?
        .clone();
    let persona = crate::agent::persona_projection::persona_projection(&config, agent_alias);
    let persona_section = persona.section.clone();
    let memory_composite = {
        use reiclaw_config::multi_agent::MemoryBackendKind;
        match agent.memory.backend {
            MemoryBackendKind::Markdown => format!("markdown.{agent_alias}"),
            MemoryBackendKind::None => "none".to_string(),
            _ => {
                let raw = config.memory.backend.trim();
                if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
                    "none".to_string()
                } else {
                    let (kind, alias) = raw.split_once('.').unwrap_or((raw, "default"));
                    format!("{kind}.{alias}")
                }
            }
        }
    };
    let __zc_alias = agent_alias.to_string();
    let __zc_message = message.to_string();
    let __zc_session_id = session_id.map(str::to_string);
    let __zc_attribution_span =
        ::reiclaw_log::attribution_span!(&crate::agent::AgentAttribution(__zc_alias.as_str()));
    let __zc_scope_span = ::reiclaw_log::info_span!(
        target: "zeroclaw_log_internal_scope",
        "zeroclaw_scope",
        risk_profile = %agent.risk_profile,
        runtime_profile = %agent.runtime_profile,
        memory_namespace = %memory_composite,
    );
    let __zc_body = async move {
        let agent_alias: &str = __zc_alias.as_str();
        let message: &str = __zc_message.as_str();
        let session_id: Option<&str> = __zc_session_id.as_deref();

        // ── Effective per-agent runtime tunables ──────────────────────
        // Profile values (when set) override the agent's inline fields.
        // See `Config::resolved_agent_config` for precedence rules.
        let eff_compact_context = agent.resolved.compact_context;
        let eff_max_system_prompt_chars = agent.resolved.max_system_prompt_chars;
        let eff_prompt_injection_mode = agent.resolved.prompt_injection_mode;

        let observer: Arc<dyn Observer> =
            Arc::from(observability::create_observer(&config.observability));
        let runtime: Arc<dyn platform::RuntimeAdapter> =
            Arc::from(platform::create_runtime(&config.runtime)?);
        let task_identity = task_identity.or_else(|| {
            runtime
                .has_filesystem_access()
                .then(|| crate::tools::task_context::TaskIdentity::capture(&config, agent_alias))
        });
        let security = Arc::new(SecurityPolicy::for_agent(&config, agent_alias)?);
        let (provider_name, provider_alias, agent_model_provider) = match config
            .resolved_model_provider_for_agent(agent_alias)
        {
            Some(resolved) => (resolved.0, resolved.1.to_string(), Some(resolved.2.clone())),
            None => {
                let agent_ref = agent.model_provider.as_str();
                if !agent_ref.is_empty() {
                    anyhow::bail!(
                        "agents.{agent_alias}.model_provider = \"{agent_ref}\" does not resolve to \
                     a configured [providers.models.<type>.<alias>] entry"
                    );
                }
                anyhow::bail!(
                    "agents.{agent_alias}.model_provider is empty \u{2014} set it to a configured \
                 \"<type>.<alias>\" (e.g. \"anthropic.{agent_alias}\")"
                );
            }
        };
        let approval_manager =
            ApprovalManager::for_non_interactive(&risk_profile).with_store_at(&config.data_dir);
        let mem: Arc<dyn Memory> = reiclaw_memory::create_memory_for_agent(
            &config,
            agent_alias,
            agent_model_provider
                .as_ref()
                .and_then(|e| e.api_key.as_deref()),
        )
        .await?;

        let all_tools_result_pm = tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            &risk_profile,
            agent_alias,
            runtime.clone(),
            mem.clone(),
            &config.http_request,
            &config.web_fetch,
            &config.agents,
            agent_model_provider
                .as_ref()
                .and_then(|e| e.api_key.as_deref()),
            &config,
            None,
            None,
            // Gateway/WS and ACP turn seeding is a top-level origin: the
            // turn's own lineage root, no inherited one.
            None,
            task_identity,
        );
        let skills = crate::skills::load_skills_for_agent_from_config(&config, agent_alias);
        let assembled = scoped::ScopedToolRegistry::assemble(scoped::ScopedAssembly {
            config: &config,
            agent_alias,
            security: &security,
            built: all_tools_result_pm,
            skills: &skills,
            runtime: runtime.clone(),
            caller_allowed: None,
            connect_mcp: true,
            exclude_memory: false,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: true,
            // `process_message` is the channel/orchestrator live-chat path;
            // it has no cross-turn reuse contract, so the per-call
            // `connect_all` path inside `assemble` is the correct choice.
            // The daemon heartbeat worker — the only caller that has a
            // reuse contract — passes its own `mcp_registry` through
            // `agent::run` (`AgentRunOverrides::mcp_registry`).
            mcp_registry: None,
        })
        .await;
        // process_message injects one combined MCP prompt block: deferred tool-search
        // listing + pinned resources, composed by the harness. The shared prompt
        // helper applies the text-tool policy for a non-native strict target.
        let deferred_section = assembled.combined_mcp_prompt_section();
        let scoped::ScopedAssembled {
            registry,
            task_owner,
            activated_handle: activated_handle_pm,
            mcp_tool_names: mcp_tool_names_pm,
            ..
        } = assembled;
        let tools_registry = registry.into_inner();

        let model_name = match agent_model_provider
            .as_ref()
            .and_then(|e| e.model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            Some(m) => m.to_string(),
            None => anyhow::bail!(
                "agents.{agent_alias}.model_provider resolves to a model_provider entry with no \
             `model` set. Configure [providers.models.{provider_name}.<alias>] model = \"...\"."
            ),
        };
        let provider_runtime_options = reiclaw_providers::provider_runtime_options_for_alias(
            &config,
            provider_name,
            provider_alias.as_str(),
        );
        let model_provider: Box<dyn ModelProvider> =
            reiclaw_providers::create_routed_model_provider_with_options(
                &config,
                &format!("{provider_name}.{provider_alias}"),
                agent_model_provider
                    .as_ref()
                    .and_then(|e| e.api_key.as_deref()),
                agent_model_provider.as_ref().and_then(|e| e.uri.as_deref()),
                &config.reliability,
                &config.model_routes,
                &model_name,
                &provider_runtime_options,
            )?;

        let mut tool_descs: Vec<(&str, &str)> = vec![
            ("shell", "Execute terminal commands."),
            ("file_read", "Read file contents."),
            ("file_write", "Write file contents."),
            ("memory_store", "Save to memory."),
            ("memory_recall", "Search memory."),
            ("memory_forget", "Delete a memory entry."),
            ("screenshot", "Capture a screenshot."),
        ];
        if matches!(
            eff_prompt_injection_mode,
            reiclaw_config::schema::SkillsPromptInjectionMode::Compact
        ) {
            tool_descs.push((
                "read_skill",
                "Load the full source for an available skill by name.",
            ));
        }

        let mut excluded_tools = compute_excluded_mcp_tools(
            &tools_registry,
            &agent.resolved.tool_filter_groups,
            message,
            &mcp_tool_names_pm,
        );
        {
            let active_profile = &risk_profile;
            if active_profile.level != AutonomyLevel::Full {
                excluded_tools.extend(active_profile.excluded_tools.iter().cloned());
            }
        }

        let bootstrap_max_chars = if eff_compact_context {
            Some(crate::agent::system_prompt::COMPACT_BOOTSTRAP_MAX_CHARS)
        } else {
            None
        };
        let effective_message = message.to_string();
        let thinking_level = agent.resolved.thinking.default_level;
        let thinking_params = crate::agent::thinking::apply_thinking_level_with_config(
            thinking_level,
            &agent.resolved.thinking,
        );
        let effective_temperature: Option<f64> = agent_model_provider
            .as_ref()
            .and_then(|e| e.temperature)
            .map(|t| {
                crate::agent::thinking::clamp_temperature(
                    t + thinking_params.temperature_adjustment,
                )
            });

        let agent_workspace = config.agent_workspace_dir(agent_alias);
        let system_prompt = build_system_prompt_for_turn(
            &agent_workspace,
            &model_name,
            &tool_descs,
            &deferred_section,
            &skills,
            Some(&agent.identity),
            bootstrap_max_chars,
            &risk_profile,
            model_provider.as_ref(),
            &tools_registry,
            &excluded_tools,
            activated_handle_pm.as_ref(),
            agent.resolved.strict_tool_parsing,
            eff_prompt_injection_mode,
            eff_compact_context,
            eff_max_system_prompt_chars,
            false,
            config.channels.show_tool_calls,
            persona_section.as_deref(),
            persona.legacy_files,
            thinking_params.system_prompt_prefix.as_deref(),
        )?;

        // Memory context is injected once in the engine, keyed on the ingress
        // origin (agent::memory_inject); recall is scoped to this entry's
        // session_id.
        // Pre-mint the turn id so the agent_turn bracket carries one
        // correlation id.
        let turn_id = uuid::Uuid::new_v4().to_string();
        let now = jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S %:z");
        let enriched = format!("[{now}] {effective_message}");
        let mut history = vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&enriched),
        ];
        let loop_knobs = LoopKnobs::default();
        // Keep the direct-message entry's previous pacing until its config
        // wiring is validated separately from this assembly refactor.
        let pacing = reiclaw_config::schema::PacingConfig::default();
        let exec = ResolvedAgentExecution::resolve(
            ResolvedModelAccess {
                model_provider: model_provider.as_ref(),
                provider_name,
                model: &model_name,
                temperature: effective_temperature,
            },
            ResolvedIo {
                tools_registry: &tools_registry,
                observer: observer.as_ref(),
                silent: true,
                approval: Some(&approval_manager),
                multimodal_config: &config.multimodal,
                config: Some(&config),
                hooks: hooks.as_deref(),
                activated_tools: activated_handle_pm.as_ref(),
                receipt_generator: None,
            },
            ResolvedRuntimeKnobs {
                excluded_tools: &excluded_tools,
                // This entry historically uses the raw resolved ceiling;
                // CLI run uses effective_context_budget through for_agent.
                context_token_budget: agent.resolved.max_context_tokens,
                ..ResolvedRuntimeKnobs::for_agent(&agent.resolved, &pacing, &loop_knobs)
            },
        )
        .with_task_owner(task_owner.as_ref());
        let mut turn = ToolLoop::for_turn(
            exec,
            &mut history,
            crate::agent::TurnMeta {
                channel_name: "daemon",
                agent_alias: Some(agent_alias),
                parent_agent_alias: None,
                turn_id: &turn_id,
            },
            IngressContext::from_origin(origin),
        );
        // This direct-message entry has no approval back-channel; gated tools
        // retain the non-interactive auto-deny from the resolved I/O above.
        turn.memory = Some(crate::agent::memory_inject::TurnMemory {
            handle: mem.as_ref(),
            query: effective_message.clone(),
            sessions: vec![session_id.map(str::to_string)],
            suppress: false,
            cfg: crate::agent::memory_inject::MemoryInjectConfig::from_memory_config(
                &config.memory,
                crate::agent::memory_inject::DEFAULT_RECALL_LIMIT,
            ),
        });
        reiclaw_api::NATIVE_THINKING_OVERRIDE
            .scope(
                thinking_params.native_thinking,
                super::agent_turn::agent_turn(turn, Some(security.as_ref())),
            )
            .await
    };
    __zc_body
        .instrument(__zc_scope_span)
        .instrument(__zc_attribution_span)
        .await
}
