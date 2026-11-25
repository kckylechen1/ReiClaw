use std::time::Duration;

/// Token usage breakdown for a single agent turn.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnTokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Discrete events emitted by the agent runtime for observability.
///
/// Each variant represents a lifecycle event that observers can record,
/// aggregate, or forward to external monitoring systems. Events carry
/// just enough context for tracing and diagnostics without exposing
/// sensitive prompt or response content.
///
/// Marked `#[non_exhaustive]` so out-of-tree observer implementations
/// degrade gracefully when new variants are added in future minor
/// releases — they must include a wildcard arm in their `match`
/// expressions and will simply ignore unknown event kinds.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ObserverEvent {
    /// The agent orchestration loop has started a new session.
    AgentStart {
        model_provider: String,
        model: String,
        channel: Option<String>,
        agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// A request is about to be sent to an LLM model_provider.
    ///
    /// This is emitted immediately before a model_provider call so observers can print
    /// user-facing progress without leaking prompt contents.
    LlmRequest {
        model_provider: String,
        model: String,
        messages_count: usize,
        channel: Option<String>,
        agent_alias: Option<String>,
        /// The delegating agent's alias when this record was produced by a
        /// nested cross-agent execution (e.g. a live SOP step that named a
        /// different agent): `agent_alias` is the EFFECTIVE agent whose
        /// policy/tools executed; this preserves the parent correlation.
        /// `None` for ordinary single-agent turns.
        parent_agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// Result of a single LLM model_provider call.
    LlmResponse {
        model_provider: String,
        model: String,
        duration: Duration,
        success: bool,
        error_message: Option<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        channel: Option<String>,
        agent_alias: Option<String>,
        /// The delegating agent's alias when this record was produced by a
        /// nested cross-agent execution (e.g. a live SOP step that named a
        /// different agent): `agent_alias` is the EFFECTIVE agent whose
        /// policy/tools executed; this preserves the parent correlation.
        /// `None` for ordinary single-agent turns.
        parent_agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// The agent session has finished.
    ///
    /// Carries aggregate usage data (tokens, cost) when the model_provider reports it.
    AgentEnd {
        model_provider: String,
        model: String,
        duration: Duration,
        tokens_used: Option<TurnTokenUsage>,
        cost_usd: Option<f64>,
        channel: Option<String>,
        agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// A tool call is about to be executed.
    ToolCallStart {
        tool: String,
        /// Provider-assigned tool call identifier, when the underlying tool
        /// call originated from a native structured tool call block (e.g.
        /// OpenAI `tool_calls[].id`, Anthropic `tool_use.id`). `None` for
        /// text-parsed (XML/markdown) tool calls.
        ///
        /// Observers can correlate `ToolCallStart` → `ToolCall` → the
        /// emitting LLM response via this id.
        tool_call_id: Option<String>,
        /// Full JSON arguments the agent passed to the tool. `None` when
        /// arguments are unavailable at the call site.
        arguments: Option<String>,
        channel: Option<String>,
        agent_alias: Option<String>,
        /// The delegating agent's alias when this record was produced by a
        /// nested cross-agent execution (e.g. a live SOP step that named a
        /// different agent): `agent_alias` is the EFFECTIVE agent whose
        /// policy/tools executed; this preserves the parent correlation.
        /// `None` for ordinary single-agent turns.
        parent_agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// A tool call has completed with a success/failure outcome.
    ToolCall {
        tool: String,
        /// Provider-assigned tool call identifier, when present. See
        /// [`ObserverEvent::ToolCallStart::tool_call_id`].
        tool_call_id: Option<String>,
        duration: Duration,
        success: bool,
        /// Full JSON arguments the agent passed to the tool.
        ///
        /// Carried here (in addition to `ToolCallStart`) so observers that
        /// build a single completed span per tool call — e.g. the OTel
        /// exporter — can attach arguments at span-end time without holding
        /// per-call state.
        arguments: Option<String>,
        /// Scrubbed tool output or error reason. Populated for both success
        /// and failure outcomes so backends can show the actual tool result
        /// in trace viewers. Credentials are scrubbed before this field is
        /// emitted.
        result: Option<String>,
        channel: Option<String>,
        agent_alias: Option<String>,
        /// The delegating agent's alias when this record was produced by a
        /// nested cross-agent execution (e.g. a live SOP step that named a
        /// different agent): `agent_alias` is the EFFECTIVE agent whose
        /// policy/tools executed; this preserves the parent correlation.
        /// `None` for ordinary single-agent turns.
        parent_agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// A memory recall (search) operation has completed.
    ///
    /// Emitted at the runtime boundary after a hybrid-search query against
    /// the brain DB. Carries an optional `query_summary` for diagnostics —
    /// scrubbed and truncated user text, not a privacy-clean token. The
    /// runtime applies `scrub_credentials` first and then truncates to ≤200
    /// content chars (with a 3-char `...` ellipsis appended when truncation
    /// occurred); short non-credential queries pass through unchanged.
    MemoryRecall {
        /// Scrubbed and truncated query summary. `None` when the caller has
        /// no meaningful query to record (e.g., session-scoped fetches).
        query_summary: Option<String>,
        duration: Duration,
        /// Number of `MemoryEntry` rows returned by the recall call.
        num_entries: usize,
        /// Bounded backend identifier (e.g. `"sqlite"`, `"qdrant"`, `"none"`).
        backend: String,
        success: bool,
        channel: Option<String>,
        agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// A memory store (write) operation has completed.
    ///
    /// Emitted after persisting a memory entry. Carries only bounded
    /// fields — the raw memory `key` is intentionally omitted because
    /// keys can encode high-cardinality identifiers (UUIDs, phone
    /// numbers, message timestamps) that would create unbounded event dimensions.
    MemoryStore {
        /// Memory category (`"core"`, `"daily"`, `"conversation"`, etc.) —
        /// bounded set for event grouping.
        category: String,
        /// Bounded backend identifier (e.g. `"sqlite"`, `"qdrant"`).
        backend: String,
        duration: Duration,
        success: bool,
        channel: Option<String>,
        agent_alias: Option<String>,
        turn_id: Option<String>,
    },
    /// The agent produced a final answer for the current user message.
    TurnComplete,
    /// Periodic heartbeat tick from the runtime keep-alive loop.
    HeartbeatTick,
    /// Response cache hit — an LLM call was avoided.
    CacheHit {
        /// `"hot"` (in-memory) or `"warm"` (SQLite).
        cache_type: String,
        /// Estimated tokens saved by this cache hit.
        tokens_saved: u64,
    },
    /// Response cache miss — the prompt was not found in cache.
    CacheMiss {
        /// `"response"` cache layer that was checked.
        cache_type: String,
    },
    /// An error occurred in a named component.
    Error {
        /// Subsystem where the error originated (e.g., `"model_provider"`, `"gateway"`).
        component: String,
        /// Human-readable error description. Must not contain secrets or tokens.
        message: String,
    },
    /// The agent trimmed oldest whole turns from history to fit either the
    /// context token budget or the configured message limit. Carries the cut
    /// accounting so dashboards and clients can surface a visible "context was
    /// trimmed" signal instead of the agent silently losing earlier turns.
    HistoryTrimmed {
        dropped_messages: usize,
        kept_turns: usize,
        reason: String,
        channel: Option<String>,
        agent_alias: Option<String>,
        turn_id: Option<String>,
    },
}

/// Core observability trait for recording agent runtime telemetry.
///
/// Implement this trait to integrate with any monitoring backend (structured
/// logging or event broadcasts). The agent runtime holds one or
/// more `Observer` instances and calls [`record_event`](Observer::record_event)
/// at key lifecycle points.
///
/// Implementations must be `Send + Sync + 'static` because the observer is
/// shared across async tasks via `Arc`.
pub trait Observer: Send + Sync + 'static {
    /// Record a discrete lifecycle event.
    ///
    /// Called synchronously on the hot path; implementations should avoid
    /// blocking I/O.
    fn record_event(&self, event: &ObserverEvent);

    /// Return the human-readable name of this observer backend.
    ///
    /// Used in logs and diagnostics (e.g., `"log"`, `"verbose"`, `"noop"`).
    fn name(&self) -> &str;
}

/// Blanket implementation: `Arc<T>` delegates all `Observer` methods to `T`.
///
/// Lets a singleton observer be handed out as `Arc<MyObserver>` and still be
/// used wherever `Box<dyn Observer>` is expected (e.g.
/// `Box::new(MyObserver::shared())`).
impl<T: Observer + ?Sized> Observer for std::sync::Arc<T> {
    fn record_event(&self, event: &ObserverEvent) {
        self.as_ref().record_event(event);
    }

    fn name(&self) -> &str {
        self.as_ref().name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct DummyObserver {
        events: Mutex<u64>,
    }

    impl Observer for DummyObserver {
        fn record_event(&self, _event: &ObserverEvent) {
            let mut guard = self.events.lock();
            *guard += 1;
        }

        fn name(&self) -> &str {
            "dummy-observer"
        }
    }

    #[test]
    fn observer_records_events_and_reports_name() {
        let observer = DummyObserver::default();

        observer.record_event(&ObserverEvent::HeartbeatTick);
        observer.record_event(&ObserverEvent::Error {
            component: "test".into(),
            message: "boom".into(),
        });

        assert_eq!(*observer.events.lock(), 2);
        assert_eq!(observer.name(), "dummy-observer");
    }

    #[test]
    fn observer_events_carry_turn_metadata_and_structured_usage() {
        let usage = TurnTokenUsage {
            input_tokens: 12,
            output_tokens: 34,
        };

        let event = ObserverEvent::AgentEnd {
            model_provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            duration: Duration::from_millis(50),
            tokens_used: Some(usage.clone()),
            cost_usd: Some(0.001),
            channel: Some("wss".into()),
            agent_alias: Some("default".into()),
            turn_id: Some("turn-1".into()),
        };

        match event {
            ObserverEvent::AgentEnd {
                tokens_used,
                channel,
                agent_alias,
                turn_id,
                ..
            } => {
                let tokens = tokens_used.expect("usage should be present");
                assert_eq!(tokens.input_tokens, 12);
                assert_eq!(tokens.output_tokens, 34);
                assert_eq!(channel.as_deref(), Some("wss"));
                assert_eq!(agent_alias.as_deref(), Some("default"));
                assert_eq!(turn_id.as_deref(), Some("turn-1"));
            }
            _ => panic!("expected AgentEnd"),
        }
    }

    #[test]
    fn observer_event_is_cloneable() {
        let event = ObserverEvent::ToolCall {
            tool: "shell".into(),
            tool_call_id: Some("call_abc123".into()),
            duration: Duration::from_millis(10),
            success: true,
            arguments: Some(r#"{"command":"date"}"#.into()),
            result: Some("Mon Apr 22 12:00:00 UTC 2026\n".into()),
            channel: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: None,
        };

        let cloned_event = event.clone();

        assert!(matches!(cloned_event, ObserverEvent::ToolCall { .. }));
    }

    #[test]
    fn memory_event_variants_are_cloneable() {
        let recall = ObserverEvent::MemoryRecall {
            query_summary: Some("user asked about coffee orders".into()),
            duration: Duration::from_millis(40),
            num_entries: 3,
            backend: "sqlite".into(),
            success: true,
            channel: Some("cli".into()),
            agent_alias: Some("default".into()),
            turn_id: Some("turn-1".into()),
        };
        let store = ObserverEvent::MemoryStore {
            category: "conversation".into(),
            backend: "sqlite".into(),
            duration: Duration::from_millis(8),
            success: true,
            channel: Some("cli".into()),
            agent_alias: Some("default".into()),
            turn_id: Some("turn-1".into()),
        };

        match recall.clone() {
            ObserverEvent::MemoryRecall { turn_id, .. } => {
                assert_eq!(turn_id.as_deref(), Some("turn-1"));
            }
            other => panic!("clone changed variant: {other:?}"),
        }
        assert!(matches!(store.clone(), ObserverEvent::MemoryStore { .. }));
    }
}
