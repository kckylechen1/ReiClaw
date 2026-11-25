use super::traits::{Observer, ObserverEvent};

/// Log-based observer — uses tracing, zero external deps
pub struct LogObserver;

impl Default for LogObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl LogObserver {
    pub fn new() -> Self {
        Self
    }
}

impl Observer for LogObserver {
    fn record_event(&self, event: &ObserverEvent) {
        match event {
            ObserverEvent::AgentStart {
                model_provider,
                model,
                channel: _,
                agent_alias: _,
                turn_id: _,
            } => {
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_attrs(
                            ::serde_json::json!({"model_provider": model_provider, "model": model})
                        ),
                    "agent.start"
                );
            }
            ObserverEvent::AgentEnd {
                model_provider,
                model,
                duration,
                tokens_used,
                cost_usd,
                channel: _,
                agent_alias: _,
                turn_id: _,
            } => {
                let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": model_provider, "model": model, "duration_ms": ms, "tokens": tokens_used, "cost_usd": cost_usd})), "agent.end");
            }
            ObserverEvent::ToolCallStart { tool, .. } => {
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"tool": tool})),
                    "tool.start"
                );
            }
            ObserverEvent::ToolCall {
                tool,
                duration,
                success,
                ..
            } => {
                let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"tool": tool, "duration_ms": ms, "success": success})), "tool.call");
            }
            ObserverEvent::TurnComplete => {
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note),
                    "turn.complete"
                );
            }
            ObserverEvent::HeartbeatTick => {
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note),
                    "heartbeat.tick"
                );
            }
            ObserverEvent::CacheHit {
                cache_type,
                tokens_saved,
            } => {
                ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"cache_type": cache_type, "tokens_saved": tokens_saved})), "cache.hit");
            }
            ObserverEvent::CacheMiss { cache_type } => {
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"cache_type": cache_type})),
                    "cache.miss"
                );
            }
            ObserverEvent::Error { component, message } => {
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_attrs(
                            ::serde_json::json!({"component": component, "error": message})
                        ),
                    "error"
                );
            }
            ObserverEvent::MemoryRecall {
                duration,
                num_entries,
                backend,
                success,
                ..
            } => {
                // Bounded labels only — `query_summary` intentionally omitted
                // from the log surface to avoid forwarding scrubbed-but-still-
                // identifying user query text into structured-log sinks
                // (Datadog, Loki) that the OD4 analysis did not cover.
                let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "backend": backend,
                            "num_entries": num_entries,
                            "duration_ms": ms,
                            "success": success
                        })),
                    "memory.recall"
                );
            }
            ObserverEvent::MemoryStore {
                category,
                backend,
                duration,
                success,
                ..
            } => {
                let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                ::reiclaw_log::record!(
                    INFO,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "category": category,
                            "backend": backend,
                            "duration_ms": ms,
                            "success": success
                        })),
                    "memory.store"
                );
            }
            ObserverEvent::LlmRequest {
                model_provider,
                model,
                messages_count,
                channel: _,
                agent_alias: _,
                parent_agent_alias: _,
                turn_id: _,
            } => {
                ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": model_provider, "model": model, "messages_count": messages_count})), "llm.request");
            }
            ObserverEvent::LlmResponse {
                model_provider,
                model,
                duration,
                success,
                error_message,
                input_tokens,
                output_tokens,
                ..
            } => {
                let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": model_provider, "model": model, "duration_ms": ms, "success": success, "error": error_message, "input_tokens": input_tokens, "output_tokens": output_tokens})), "llm.response");
            }
            // `ObserverEvent` is `#[non_exhaustive]` — silently ignore any
            // future variant added by upstream `zeroclaw-api`.
            _ => {}
        }
    }

    fn name(&self) -> &str {
        "log"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn log_observer_name() {
        assert_eq!(LogObserver::new().name(), "log");
    }

    #[test]
    fn log_observer_all_events_no_panic() {
        let obs = LogObserver::new();
        obs.record_event(&ObserverEvent::AgentStart {
            model_provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            channel: None,
            agent_alias: None,
            turn_id: None,
        });
        obs.record_event(&ObserverEvent::AgentEnd {
            model_provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::from_millis(500),
            tokens_used: None,
            cost_usd: Some(0.0015),
            channel: None,
            agent_alias: None,
            turn_id: None,
        });
        obs.record_event(&ObserverEvent::AgentEnd {
            model_provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::ZERO,
            tokens_used: None,
            cost_usd: None,
            channel: None,
            agent_alias: None,
            turn_id: None,
        });
        obs.record_event(&ObserverEvent::LlmResponse {
            parent_agent_alias: None,
            model_provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::from_millis(150),
            success: true,
            error_message: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            channel: None,
            agent_alias: None,
            turn_id: None,
        });
        obs.record_event(&ObserverEvent::LlmResponse {
            parent_agent_alias: None,
            model_provider: "openrouter".into(),
            model: "claude-sonnet".into(),
            duration: Duration::from_millis(200),
            success: false,
            error_message: Some("rate limited".into()),
            input_tokens: None,
            output_tokens: None,
            channel: None,
            agent_alias: None,
            turn_id: None,
        });
        obs.record_event(&ObserverEvent::ToolCall {
            parent_agent_alias: None,
            tool: "shell".into(),
            tool_call_id: None,
            duration: Duration::from_millis(10),
            success: false,
            arguments: None,
            result: None,
            channel: None,
            agent_alias: None,
            turn_id: None,
        });
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_event(&ObserverEvent::Error {
            component: "model_provider".into(),
            message: "timeout".into(),
        });
    }
}
