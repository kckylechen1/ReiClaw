//! Turn-loop control-flow outcomes and dispatched model attribution.

/// Runtime request route of a successful model call. This does not inspect
/// retries or fallback providers hidden inside the selected provider wrapper.
#[derive(Clone, Debug)]
pub(crate) struct DispatchedModelRoute {
    pub(crate) provider_name: String,
    pub(crate) model: String,
}

impl DispatchedModelRoute {
    pub(crate) fn selected(
        config: Option<&zeroclaw_config::schema::Config>,
        provider_name: &str,
        model: &str,
    ) -> Self {
        // Config owns the same hint table used to construct RouterModelProvider.
        // Duplicate hints use their last entry, matching its HashMap collection.
        let route = model.strip_prefix("hint:").and_then(|hint| {
            config?
                .model_routes
                .iter()
                .rev()
                .find(|route| route.hint == hint)
        });
        Self {
            provider_name: route
                .map_or(provider_name, |route| &route.model_provider)
                .to_string(),
            model: route.map_or(model, |route| &route.model).to_string(),
        }
    }
}

/// Owned by one streamed API turn, across its steering rounds.
pub(crate) type DispatchedModelRouteSink = parking_lot::Mutex<Option<DispatchedModelRoute>>;

#[derive(Debug)]
pub struct ToolLoopCancelled;

impl std::fmt::Display for ToolLoopCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tool loop cancelled")
    }
}

impl std::error::Error for ToolLoopCancelled {}

/// The caller owns steering admission and enrichment. This is a continuation,
/// not a failed turn; all tool results have already entered both transcripts.
#[derive(Debug)]
pub(crate) struct SteeringPending {
    pub(crate) display_text: String,
}

impl std::fmt::Display for SteeringPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("steering pending at tool boundary")
    }
}

impl std::error::Error for SteeringPending {}

pub fn is_tool_loop_cancelled(err: &anyhow::Error) -> bool {
    err.chain().any(|source| source.is::<ToolLoopCancelled>())
}

#[derive(Debug)]
pub(crate) struct StreamInterruptedAfterOutput {
    pub(crate) partial_text: String,
    pub(crate) message: String,
}

impl std::fmt::Display for StreamInterruptedAfterOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StreamInterruptedAfterOutput {}

#[derive(Debug)]
pub(crate) struct StreamCancelledAfterOutput {
    pub(crate) partial_text: String,
    cause: ToolLoopCancelled,
}

impl StreamCancelledAfterOutput {
    pub(crate) fn new(partial_text: String) -> Self {
        Self {
            partial_text,
            cause: ToolLoopCancelled,
        }
    }
}

impl std::fmt::Display for StreamCancelledAfterOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tool loop cancelled after streamed output")
    }
}

impl std::error::Error for StreamCancelledAfterOutput {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_loop_cancelled_display() {
        let err = ToolLoopCancelled;
        assert_eq!(err.to_string(), "tool loop cancelled");
    }

    #[test]
    fn is_tool_loop_cancelled_direct() {
        let err = anyhow::Error::new(ToolLoopCancelled);
        assert!(is_tool_loop_cancelled(&err));
    }

    #[test]
    fn is_tool_loop_cancelled_unrelated_error_returns_false() {
        let err = anyhow::Error::msg("some other error");
        assert!(!is_tool_loop_cancelled(&err));
    }

    #[test]
    fn stream_cancelled_after_output_display() {
        let e = StreamCancelledAfterOutput::new("partial text".to_string());
        assert_eq!(e.to_string(), "tool loop cancelled after streamed output");
        assert_eq!(e.partial_text, "partial text");
    }

    #[test]
    fn stream_cancelled_after_output_source_chains_to_tool_loop_cancelled() {
        use std::error::Error;
        let e = StreamCancelledAfterOutput::new(String::new());
        let source = e.source().expect("must have source");
        assert!(source.is::<ToolLoopCancelled>());
    }

    #[test]
    fn is_tool_loop_cancelled_recognizes_stream_cancelled_after_output() {
        let e = anyhow::Error::new(StreamCancelledAfterOutput::new("txt".to_string()));
        assert!(is_tool_loop_cancelled(&e));
    }
}
