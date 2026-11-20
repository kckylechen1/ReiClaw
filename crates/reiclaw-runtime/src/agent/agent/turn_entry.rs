//! Agent::turn / turn_streamed entry points extracted from `agent.rs`.

use super::*;
use crate::observability::ObserverEvent;
use anyhow::Result;
use reiclaw_api::agent::TurnEvent;
use reiclaw_providers::{ChatMessage, ConversationMessage};

struct PreparedProviderMessages {
    messages: Vec<ChatMessage>,
    vision_provider: Option<crate::agent::turn::vision_route::ResolvedVisionProvider>,
}

#[derive(Clone, Copy)]
enum TurnMode {
    Direct,
    Streamed,
}

impl TurnMode {
    fn is_streamed(self) -> bool {
        matches!(self, Self::Streamed)
    }

    fn knobs(self) -> crate::agent::loop_::LoopKnobs {
        use crate::agent::loop_::MaxIterationBehavior;
        crate::agent::loop_::LoopKnobs {
            dedup_enabled: false,
            max_iteration_behavior: match self {
                Self::Direct => MaxIterationBehavior::ErrorAtCap,
                Self::Streamed => MaxIterationBehavior::GracefulSummary,
            },
            detect_protocol_without_tools: false,
        }
    }

    fn approval_bridge(self, agent: &Agent) -> Option<Box<dyn reiclaw_api::channel::Channel>> {
        if !self.is_streamed() {
            return None;
        }
        agent.channel_handles.ask_user.as_ref().map(|handles| {
            Box::new(crate::agent::approval_bridge::AskUserApprovalBridge::new(
                Arc::clone(handles),
                agent.approval_route.clone(),
            )) as Box<dyn reiclaw_api::channel::Channel>
        })
    }
}

impl Agent {
    fn validate_user_message(user_message: &str, entry_point: &str) -> Result<()> {
        if !user_message.trim().is_empty() {
            return Ok(());
        }
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                .with_category(::reiclaw_log::EventCategory::Agent)
                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "reason": "empty_user_message",
                    "entry_point": entry_point,
                    "raw_len": user_message.len(),
                })),
            "Refusing blank user turn (would emit timestamp-only message and risk prompt-template bleed-through)"
        );
        anyhow::bail!("empty user message: refusing to dispatch a blank turn")
    }

    async fn initialize_turn_history(&mut self) -> Result<()> {
        self.assemble_turn_context().await;
        if self.history.is_empty() {
            let system_prompt = self.build_system_prompt()?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }
        Ok(())
    }

    /// Prepare one accepted user message, including mid-turn steering.
    /// Callers own transcript insertion so steering stays round-local until
    /// hooks have observed it and the completed round is replayed.
    async fn prepare_user_message(&mut self, user_message: &str, turn_id: &str) -> ChatMessage {
        // Memory context is injected once in the engine, keyed on the
        // ingress origin (agent::memory_inject).
        if self.auto_save {
            let store_start = std::time::Instant::now();
            let store_result = self
                .memory
                .store(
                    "user_msg",
                    user_message,
                    MemoryCategory::Conversation,
                    self.memory_session_id.as_deref(),
                )
                .await;
            self.observer.record_event(&ObserverEvent::MemoryStore {
                category: MemoryCategory::Conversation.to_string(),
                backend: self.memory.name().to_string(),
                duration: store_start.elapsed(),
                success: store_result.is_ok(),
                channel: Some(self.channel_name.clone()),
                agent_alias: self.observer_agent_alias(),
                turn_id: Some(turn_id.to_string()),
            });
        }

        let now = self
            .current_turn_datetime()
            .strftime("%Y-%m-%d %H:%M:%S %:z");
        ChatMessage::user(format!("[{now}] {user_message}"))
    }

    /// Select the initial vision route and render the prompt/transcript for
    /// its dispatcher. Entry points keep their own error and event handling.
    fn prepare_provider_messages(
        &mut self,
        effective_model: &str,
    ) -> Result<PreparedProviderMessages> {
        let base_messages = self.tool_dispatcher.to_provider_messages(&self.history);
        let (vision_provider, _degrade_strip_images) = crate::agent::turn::resolve_vision_provider(
            self.full_config(),
            self.model_provider.as_ref(),
            &base_messages,
            &self.multimodal_config,
            &self.model_provider_name,
            effective_model,
        )?;
        let active_provider: &dyn ModelProvider = vision_provider
            .as_ref()
            .map(|resolved| resolved.provider.as_ref())
            .unwrap_or(self.model_provider.as_ref());
        let dispatcher = tool_dispatcher_for_provider(&self.config, active_provider);
        self.rebuild_system_prompt_for_dispatcher(dispatcher.as_ref())?;
        Ok(PreparedProviderMessages {
            messages: dispatcher.to_provider_messages(&self.history),
            vision_provider,
        })
    }

    /// Enter the side-query scope only after prompt preparation succeeds. Its
    /// owned guard retains the existing cleanup on success, error and drop.
    fn activate_turn_side_query(
        &self,
        prepared: &PreparedProviderMessages,
        effective_model: &str,
        turn_id: &str,
    ) -> crate::agent::side_query::ActiveTurnSideQueryGuard {
        let guard = self.side_query.enter_turn(turn_id);
        let (provider, provider_name, model) = prepared.vision_provider.as_ref().map_or_else(
            || {
                (
                    Arc::clone(&self.model_provider),
                    self.model_provider_name.as_str(),
                    effective_model,
                )
            },
            |resolved| {
                (
                    Arc::clone(&resolved.provider),
                    resolved.provider_name.as_str(),
                    resolved.model.as_str(),
                )
            },
        );
        self.side_query.update_live_messages(
            provider,
            provider_name,
            model,
            &prepared.messages,
            Some(turn_id),
        );
        guard
    }

    /// Keep this turn's input round-local until hooks and the turn engine have
    /// processed it. Each entry point still owns durable transcript replay.
    fn split_turn_messages(mut messages: Vec<ChatMessage>) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
        let split = messages
            .iter()
            .rposition(|message| message.role == "user")
            .unwrap_or(messages.len());
        let current = messages.split_off(split);
        (messages, current)
    }

    fn record_turn_usage(
        guard: &mut crate::observability::AgentTurnGuard<'_>,
        context: &crate::agent::cost::ToolLoopCostTrackingContext,
    ) -> crate::agent::cost::TurnUsage {
        let usage = context.snapshot_turn_usage();
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            guard.set_usage(
                Some(reiclaw_api::observability_traits::TurnTokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                }),
                None,
            );
        }
        usage
    }

    pub async fn turn(&mut self, user_message: &str) -> Result<String> {
        Self::validate_user_message(user_message, "Agent::turn")?;
        let mode = TurnMode::Direct;

        self.initialize_turn_history().await?;

        let effective_model = self.classify_model(user_message);

        let turn_id = Self::new_turn_id();
        let turn_observer = Arc::clone(&self.observer);
        let mut guard = crate::observability::AgentTurnGuard::start(
            turn_observer.as_ref(),
            self.model_provider_name.clone(),
            effective_model.clone(),
            Some(self.channel_name.clone()),
            self.observer_agent_alias(),
            Some(turn_id.clone()),
        );

        let user_msg = self.prepare_user_message(user_message, &turn_id).await;
        self.history.push(ConversationMessage::Chat(user_msg));

        let prepared = match self.prepare_provider_messages(&effective_model) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = self.trim_history(Some(&turn_id));
                return Err(error);
            }
        };
        let _side_query_turn_guard =
            self.activate_turn_side_query(&prepared, &effective_model, &turn_id);
        let provider_messages = prepared.messages;
        let cache_key = self.response_cache_key_for_messages(&provider_messages, &effective_model);

        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key) {
            if let Ok(Some(cached)) = cache.get(key) {
                self.observer.record_event(&ObserverEvent::CacheHit {
                    cache_type: "response".into(),
                    tokens_saved: 0,
                });
                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(
                        cached.clone(),
                    )));
                let _ = self.trim_history(Some(&turn_id));
                return Ok(cached);
            }
            self.observer.record_event(&ObserverEvent::CacheMiss {
                cache_type: "response".into(),
            });
        }

        // Keep this turn's input separate so the hook observes it before the
        // engine appends it and the entry point replays the completed round.
        let (mut loop_history, mut loop_new_messages) =
            Self::split_turn_messages(provider_messages);

        let approval_bridge = mode.approval_bridge(self);
        let knobs = mode.knobs();
        let pacing = self.turn_pacing();
        let repetition_guard =
            std::sync::Mutex::new(crate::agent::turn::repetition::RepetitionGuard::default());

        // Keep the loop call as a plain `.await` on this task. Caller-scoped
        // task-locals (session key, cost tracking, tool choice / thinking
        // overrides) silently vanish across a spawn.
        let cost_context = self.tool_loop_cost_tracking_context();
        let receipt_scope = crate::agent::tool_receipts::ReceiptScope::from_config(
            &self.config.resolved.tool_receipts,
        );
        let agent_alias_for_loop = self.observer_agent_alias();
        let loop_result = crate::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(
                Some(cost_context.clone()),
                crate::agent::turn::run_tool_call_loop_with_guard(
                    crate::agent::loop_::ToolLoop {
                        exec: crate::agent::loop_::ResolvedAgentExecution::resolve(
                            crate::agent::loop_::ResolvedModelAccess {
                                model_provider: self.model_provider.as_ref(),
                                provider_name: &self.model_provider_name,
                                model: &effective_model,
                                temperature: self.temperature,
                            },
                            crate::agent::loop_::ResolvedIo {
                                tools_registry: &self.tools,
                                observer: self.observer.as_ref(),
                                silent: mode.is_streamed(),
                                approval: self.approval_manager.as_deref(),
                                multimodal_config: &self.multimodal_config,
                                // Inlined `full_config()` (per-field borrow) so it coexists with
                                // the `&mut self.image_cache` in this same ToolLoop expression.
                                config: self
                                    .provider_switch_config
                                    .as_ref()
                                    .and_then(|c| c.config.as_deref()),
                                hooks: self.hook_runner.as_deref(),
                                activated_tools: self.activated_tools.as_ref(),
                                receipt_generator: receipt_scope
                                    .as_ref()
                                    .map(crate::agent::tool_receipts::ReceiptScope::generator),
                            },
                            crate::agent::loop_::ResolvedRuntimeKnobs::for_agent(
                                &self.config.resolved,
                                &pacing,
                                &knobs,
                            ),
                        )
                        .with_task_owner(self.task_owner.as_ref()),
                        history: &mut loop_history,
                        channel_name: &self.channel_name,
                        channel_reply_target: None,
                        cancellation_token: None,
                        on_delta: None,
                        shared_budget: None,
                        channel: approval_bridge.as_deref(),
                        collected_receipts: receipt_scope
                            .as_ref()
                            .map(crate::agent::tool_receipts::ReceiptScope::collector),
                        event_tx: None,
                        steering: None,
                        new_messages_out: Some(&mut loop_new_messages),
                        image_cache: Some(&mut self.image_cache),
                        // Direct embedded Agent::turn call; source/transport/
                        // trust stay placeholders, not yet stamped at the edge.
                        memory: Some(crate::agent::memory_inject::TurnMemory {
                            handle: self.memory.as_ref(),
                            query: user_message.to_string(),
                            sessions: vec![self.memory_session_id.clone()],
                            suppress: false,
                            cfg: self.memory_inject_cfg,
                        }),
                        ingress: reiclaw_api::ingress::IngressContext::agent_direct(),
                        agent_alias: agent_alias_for_loop.as_deref(),
                        parent_agent_alias: None,
                        turn_id: &turn_id,
                    },
                    Some(&repetition_guard),
                    mode.is_streamed(),
                    Some((&self.side_query, &self.model_provider)),
                    None,
                    self.security_policy.as_deref(),
                ),
            )
            .await;

        // Feed the accumulated per-call usage into the AgentEnd guard before
        // any return below drops it — including the error path, which must
        // still report usage from calls that succeeded earlier in the turn.
        let usage = Self::record_turn_usage(&mut guard, &cost_context);
        // Pop the original user message (pushed before the loop) so the
        // replayed version — which includes the user message, possibly
        // modified by the hook.
        self.history.pop();
        for replayed in Self::replay_loop_messages(&loop_new_messages) {
            self.history.push(replayed);
        }
        let response = match loop_result {
            Ok(response) => response,
            Err(error) => {
                let _ = self.trim_history(Some(&turn_id));
                return Err(error);
            }
        };

        let response = self.append_receipts_block(response, receipt_scope.as_ref());

        // Store in the response cache only when the turn was a single
        // tool-free exchange (exactly one assistant message), mirroring the
        // old "no tool calls" put condition.
        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key)
            && loop_new_messages.len() == 2
            && loop_new_messages
                .last()
                .is_some_and(|m| m.role == "assistant")
        {
            #[allow(clippy::cast_possible_truncation)]
            let _ = cache.put(key, &effective_model, &response, usage.output_tokens as u32);
        }

        let _ = self.trim_history(Some(&turn_id));

        Ok(response)
    }

    pub async fn turn_streamed(
        &mut self,
        user_message: &str,
        event_tx: tokio::sync::mpsc::Sender<TurnEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(String, Vec<ConversationMessage>)> {
        // See `Agent::turn` for the rationale. Same guard: blank input would
        // push a timestamp-only user message into history and the model would
        // narrate the trailing prompt-template sentinel instead of replying.
        Self::validate_user_message(user_message, "Agent::turn_streamed")?;

        self.turn_streamed_with_steering_state(user_message, event_tx, cancel_token, None)
            .await
            .map(|outcome| (outcome.response, outcome.new_messages))
            .map_err(|err| err.error)
    }

    pub async fn turn_streamed_with_steering_state(
        &mut self,
        user_message: &str,
        event_tx: tokio::sync::mpsc::Sender<TurnEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        mut steering_rx: Option<&mut tokio::sync::mpsc::Receiver<String>>,
    ) -> std::result::Result<StreamedTurnSuccess, StreamedTurnError> {
        // See `Agent::turn` for the rationale. Same guard: blank input would
        // push a timestamp-only user message into history and the model would
        // narrate the trailing prompt-template sentinel instead of replying.
        Self::validate_user_message(user_message, "Agent::turn_streamed_with_steering_state")
            .map_err(|error| StreamedTurnError {
                error,
                committed_response: String::new(),
                new_messages: Vec::new(),
            })?;
        let mode = TurnMode::Streamed;

        self.initialize_turn_history()
            .await
            .map_err(|error| StreamedTurnError {
                error,
                committed_response: String::new(),
                new_messages: Vec::new(),
            })?;

        let mut new_msgs: Vec<ConversationMessage> = Vec::new();
        let effective_model = self.classify_model(user_message);
        let turn_id = Self::new_turn_id();
        let mut committed_response = String::new();
        // Requested-vs-served divergence for THIS turn. Source of truth is the
        // task-local record inside `reiclaw_providers::reliable`, consumed
        // once per round below; this is a per-turn transient resolved at
        // use-time, never stored on the agent.
        let mut turn_model_fallback: Option<reiclaw_providers::reliable::ProviderFallbackInfo> =
            None;
        let turn_observer = Arc::clone(&self.observer);
        let mut guard = crate::observability::AgentTurnGuard::start(
            turn_observer.as_ref(),
            self.model_provider_name.clone(),
            effective_model.clone(),
            Some(self.channel_name.clone()),
            self.observer_agent_alias(),
            Some(turn_id.clone()),
        );
        let user_msg =
            ConversationMessage::Chat(self.prepare_user_message(user_message, &turn_id).await);
        new_msgs.push(user_msg.clone());
        self.history.push(user_msg);

        let prepared = match self.prepare_provider_messages(&effective_model) {
            Ok(prepared) => prepared,
            Err(error) => {
                let notice = self.trim_history(Some(&turn_id));
                forward_history_trim_notice(&event_tx, notice).await;
                return Err(StreamedTurnError {
                    error,
                    committed_response: String::new(),
                    new_messages: new_msgs,
                });
            }
        };
        let _side_query_turn_guard =
            self.activate_turn_side_query(&prepared, &effective_model, &turn_id);
        let provider_messages = prepared.messages;
        let cache_key = self.response_cache_key_for_messages(&provider_messages, &effective_model);

        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key) {
            if let Ok(Some(cached)) = cache.get(key) {
                self.observer.record_event(&ObserverEvent::CacheHit {
                    cache_type: "response".into(),
                    tokens_saved: 0,
                });
                let cached_msg = ConversationMessage::Chat(ChatMessage::assistant(cached.clone()));
                new_msgs.push(cached_msg.clone());
                self.history.push(cached_msg);
                let notice = self.trim_history(Some(&turn_id));
                forward_history_trim_notice(&event_tx, notice).await;
                self.observer.record_event(&ObserverEvent::TurnComplete);
                committed_response.push_str(&cached);
                let route = crate::agent::turn::DispatchedModelRoute::selected(
                    self.full_config(),
                    &self.model_provider_name,
                    &effective_model,
                );
                guard.set_model_route(route.provider_name.clone(), route.model.clone());
                return Ok(StreamedTurnSuccess {
                    response: committed_response,
                    new_messages: new_msgs,
                    provider_name: route.provider_name,
                    model: route.model,
                });
            }
            self.observer.record_event(&ObserverEvent::CacheMiss {
                cache_type: "response".into(),
            });
        }

        // Split provider_messages: loop_history gets past turns, user_msg_for_loop
        // seeds round 0's round_added so the hook can observe/modify the user message.
        let (mut loop_history, user_msg_for_loop) = Self::split_turn_messages(provider_messages);

        let approval_bridge = mode.approval_bridge(self);

        let knobs = mode.knobs();
        let pacing = self.turn_pacing();
        // One guard spans every steering round in this API turn.
        let repetition_guard =
            std::sync::Mutex::new(crate::agent::turn::repetition::RepetitionGuard::default());

        let cost_context = self.tool_loop_cost_tracking_context();
        let agent_alias_for_loop = self.observer_agent_alias();

        // Built once per turn so the HMAC key is stable across steering rounds
        // and the same collector accumulates every round's receipts. `None`
        // when receipts are disabled, gated by the one shared seam.
        let receipt_scope = crate::agent::tool_receipts::ReceiptScope::from_config(
            &self.config.resolved.tool_receipts,
        );

        // ── Round loop: one tool-call-loop run per steering round ──────────
        // Local to this API turn: trimming or resetting /btw state cannot erase
        // the completed request route, and a later turn cannot inherit it.
        let dispatched_route = crate::agent::turn::DispatchedModelRouteSink::default();
        for round in 0..self.config.resolved.max_tool_iterations {
            // Early exit if the caller cancelled this turn (e.g. user abort)
            if cancel_token
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                let marker = crate::agent::INTERRUPTED_BY_USER_MARKER.to_string();
                let interruption =
                    ConversationMessage::Chat(ChatMessage::assistant(marker.clone()));
                new_msgs.push(interruption.clone());
                self.history.push(interruption);
                committed_response.push_str(&marker);
                let notice = self.trim_history(Some(&turn_id));
                forward_history_trim_notice(&event_tx, notice).await;
                return Err(StreamedTurnError {
                    error: crate::agent::loop_::ToolLoopCancelled.into(),
                    committed_response,
                    new_messages: new_msgs,
                });
            }

            let mut round_added: Vec<ChatMessage> = if round == 0 {
                user_msg_for_loop.clone()
            } else {
                Vec::new()
            };

            // Steering drain: each accepted mid-turn message becomes its own
            // enriched user turn in both transcripts before the next round.
            //
            // This claims again inside a turn that already claimed once, and
            // that is deliberate, not an oversight: a steering message is a
            // fresh user turn in everything but name, it gets its own round
            // with the provider, and any child that finished since the turn
            // started is news the model can still act on. Delivering it with
            // the message that triggered it is better than holding it back
            // until the whole turn ends.
            for steering_message in crate::agent::loop_::drain_steering_messages(&mut steering_rx) {
                // Keep steering round-local until the before-llm-call hook
                // has seen it; replay below owns durable history/new_msgs.
                round_added.push(self.prepare_user_message(&steering_message, &turn_id).await);
            }
            let round_loop = crate::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                Some(cost_context.clone()),
                crate::agent::turn::run_tool_call_loop_with_guard(
                    crate::agent::loop_::ToolLoop {
                        exec: crate::agent::loop_::ResolvedAgentExecution::resolve(
                            crate::agent::loop_::ResolvedModelAccess {
                                model_provider: self.model_provider.as_ref(),
                                provider_name: &self.model_provider_name,
                                model: &effective_model,
                                temperature: self.temperature,
                            },
                            crate::agent::loop_::ResolvedIo {
                                tools_registry: &self.tools,
                                observer: self.observer.as_ref(),
                                silent: mode.is_streamed(),
                                approval: self.approval_manager.as_deref(),
                                multimodal_config: &self.multimodal_config,
                                // Inlined `full_config()` (per-field borrow) so it coexists with
                                // the `&mut self.image_cache` in this same ToolLoop expression.
                                config: self
                                    .provider_switch_config
                                    .as_ref()
                                    .and_then(|c| c.config.as_deref()),
                                hooks: self.hook_runner.as_deref(),
                                activated_tools: self.activated_tools.as_ref(),
                                receipt_generator: receipt_scope
                                    .as_ref()
                                    .map(crate::agent::tool_receipts::ReceiptScope::generator),
                            },
                            crate::agent::loop_::ResolvedRuntimeKnobs::for_agent(
                                &self.config.resolved,
                                &pacing,
                                &knobs,
                            ),
                        )
                        .with_task_owner(self.task_owner.as_ref()),
                        history: &mut loop_history,
                        channel_name: &self.channel_name,
                        channel_reply_target: None,
                        cancellation_token: cancel_token.clone(),
                        on_delta: None,
                        shared_budget: None,
                        channel: approval_bridge.as_deref(),
                        collected_receipts: receipt_scope
                            .as_ref()
                            .map(crate::agent::tool_receipts::ReceiptScope::collector),
                        event_tx: Some(event_tx.clone()),
                        steering: steering_rx.as_deref_mut(),
                        new_messages_out: Some(&mut round_added),
                        image_cache: Some(&mut self.image_cache),
                        // Direct embedded Agent::turn call; source/transport/
                        // trust stay placeholders, not yet stamped at the edge.
                        memory: Some(crate::agent::memory_inject::TurnMemory {
                            handle: self.memory.as_ref(),
                            query: user_message.to_string(),
                            sessions: vec![self.memory_session_id.clone()],
                            suppress: false,
                            cfg: self.memory_inject_cfg,
                        }),
                        ingress: reiclaw_api::ingress::IngressContext::agent_direct(),
                        agent_alias: agent_alias_for_loop.as_deref(),
                        parent_agent_alias: None,
                        turn_id: &turn_id,
                    },
                    Some(&repetition_guard),
                    mode.is_streamed(),
                    Some((&self.side_query, &self.model_provider)),
                    Some(&dispatched_route),
                    self.security_policy.as_deref(),
                ),
            );
            // Scope the provider-fallback task-local around the round so the
            // resilient wrapper's requested-vs-served record is visible here,
            // then read it immediately (same pattern as the channels
            // orchestrator's `scope_provider_fallback` wrapping). Box::pin
            // moves the round future to the heap: nesting it inside another
            // async block otherwise grows the turn future past the tokio
            // worker stack in debug builds (observed live as a worker-thread
            // stack overflow aborting the gateway).
            let (loop_result, round_fallback) =
                reiclaw_providers::reliable::scope_provider_fallback(async {
                    let result = Box::pin(round_loop).await;
                    (
                        result,
                        reiclaw_providers::reliable::take_last_provider_fallback(),
                    )
                })
                .await;
            if round_fallback.is_some() {
                turn_model_fallback = round_fallback;
            }

            // Feed cumulative usage into the AgentEnd guard before any return
            // below drops it — the error paths must still report usage from
            // calls that succeeded earlier in the turn.
            let usage = Self::record_turn_usage(&mut guard, &cost_context);

            // round_added now contains the user message for round 0;
            // a single tool-free exchange is [user, assistant].
            let single_text_exchange = round == 0
                && round_added.len() == 2
                && round_added.first().is_some_and(|m| m.role == "user")
                && round_added.last().is_some_and(|m| m.role == "assistant");

            if round == 0 {
                self.history.pop();
                new_msgs.pop();
            }
            for replayed in Self::replay_loop_messages(&round_added) {
                new_msgs.push(replayed.clone());
                self.history.push(replayed);
            }

            match loop_result {
                Ok(response) => {
                    // Commit-before-drain: this round's assistant output is in
                    // history/new_msgs (replay above) and committed_response
                    // before any steering continuation is folded in.
                    committed_response.push_str(&response);
                    let notice = self.trim_history(Some(&turn_id));
                    forward_history_trim_notice(&event_tx, notice).await;

                    let has_more_steering =
                        steering_rx.as_deref_mut().is_some_and(|rx| !rx.is_empty());
                    let repetition_exhausted = repetition_guard
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_exhausted();
                    if has_more_steering && !repetition_exhausted {
                        continue;
                    }
                    // Refuse late senders before terminal delivery awaits.
                    // Include any input that raced the first emptiness check.
                    if let Some(rx) = steering_rx.as_deref_mut() {
                        rx.close();
                        if !rx.is_empty() && !repetition_exhausted {
                            continue;
                        }
                    }

                    // Cache put only when the turn was a single tool-free
                    // exchange, mirroring the old "no tool calls" condition.
                    if single_text_exchange
                        && let (Some(cache), Some(key)) = (&self.response_cache, &cache_key)
                    {
                        #[allow(clippy::cast_possible_truncation)]
                        let _ =
                            cache.put(key, &effective_model, &response, usage.output_tokens as u32);
                    }

                    self.observer.record_event(&ObserverEvent::TurnComplete);
                    let committed_response =
                        self.append_receipts_block(committed_response, receipt_scope.as_ref());
                    let committed_response = Self::append_model_fallback_notice(
                        committed_response,
                        turn_model_fallback.as_ref(),
                        &event_tx,
                    )
                    .await;
                    let route = dispatched_route.lock().clone().unwrap_or_else(|| {
                        crate::agent::turn::DispatchedModelRoute::selected(
                            self.full_config(),
                            &self.model_provider_name,
                            &effective_model,
                        )
                    });
                    guard.set_model_route(route.provider_name.clone(), route.model.clone());
                    return Ok(StreamedTurnSuccess {
                        response: committed_response,
                        new_messages: new_msgs,
                        provider_name: route.provider_name,
                        model: route.model,
                    });
                }
                Err(error) => {
                    if let Some(pending) =
                        error.downcast_ref::<crate::agent::turn::SteeringPending>()
                    {
                        // The existing drain above owns autosave, timestamping,
                        // user provenance and round-local before-LLM input.
                        // No tool is interrupted and no second loop is spawned.
                        committed_response.push_str(&pending.display_text);
                        continue;
                    }
                    if let Some(rx) = steering_rx.as_deref_mut() {
                        rx.close();
                    }
                    // Rebuild the committed text from the failed round's plain
                    // assistant output (e.g. a persisted stream partial) when
                    // no prior round committed anything.
                    if committed_response.is_empty() {
                        for replayed in Self::replay_loop_messages(&round_added) {
                            if let ConversationMessage::Chat(message) = &replayed
                                && message.role == "assistant"
                            {
                                committed_response.push_str(&message.content);
                            }
                        }
                    }
                    let error = if crate::agent::loop_::is_tool_loop_cancelled(&error) {
                        // When the cancel arrived after event-visible
                        // streamed text, the error itself carries the
                        // partial the loop persisted (replayed into
                        // history/new_msgs above, and into
                        // committed_response by the empty-committed
                        // rebuild). Provenance, not content sniffing:
                        // model-authored text can end with the marker
                        // literal, so suffix-matching round_added would
                        // misfire. Synthesize the bare marker only when no
                        // interruption text was persisted this round.
                        let marker = crate::agent::INTERRUPTED_BY_USER_MARKER.to_string();
                        let persisted_interruption = error
                            .downcast_ref::<crate::agent::loop_::StreamCancelledAfterOutput>()
                            .map(|cancelled| format!("{}\n\n{marker}", cancelled.partial_text));
                        match persisted_interruption {
                            Some(text) => {
                                if !committed_response.ends_with(&marker) {
                                    if !committed_response.is_empty() {
                                        committed_response.push_str("\n\n");
                                    }
                                    committed_response.push_str(&text);
                                }
                            }
                            None => {
                                committed_response.push_str(&marker);
                                let interruption = ConversationMessage::Chat(
                                    ChatMessage::assistant(marker.clone()),
                                );
                                new_msgs.push(interruption.clone());
                                self.history.push(interruption);
                            }
                        }
                        crate::agent::loop_::ToolLoopCancelled.into()
                    } else {
                        // Mark the interruption only when nothing was committed —
                        // prior-round text must round-trip unmodified.
                        if committed_response.is_empty() {
                            committed_response.push_str(crate::agent::STREAM_INTERRUPTED_MARKER);
                        }
                        error
                    };
                    let notice = self.trim_history(Some(&turn_id));
                    forward_history_trim_notice(&event_tx, notice).await;
                    return Err(StreamedTurnError {
                        error,
                        committed_response,
                        new_messages: new_msgs,
                    });
                }
            }
        }

        let notice = self.trim_history(Some(&turn_id));
        forward_history_trim_notice(&event_tx, notice).await;
        Err(StreamedTurnError {
            error: anyhow::Error::msg(format!(
                "Agent exceeded maximum tool iterations ({})",
                self.config.resolved.max_tool_iterations
            )),
            committed_response,
            new_messages: new_msgs,
        })
    }
}
