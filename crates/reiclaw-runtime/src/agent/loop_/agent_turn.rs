//! Lifecycle bracket for a caller-resolved non-streaming turn.
//!
//! The entry point owns the turn id, ingress and execution bundle. This adapter
//! adds AgentStart/AgentEnd without resolving policy or rebuilding that bundle.

use anyhow::Result;

use crate::agent::cost::TOOL_LOOP_COST_TRACKING_CONTEXT;
use crate::agent::turn::{ToolLoop, run_tool_call_loop_with_security};

/// Run one resolved turn with exactly one lifecycle bracket. Callers that
/// already opened a bracket (CLI `run`) invoke the engine with the same
/// `ToolLoop` bundle directly so prompt/setup failures keep their old owner.
pub async fn agent_turn(
    turn: ToolLoop<'_>,
    security: Option<&crate::security::SecurityPolicy>,
) -> Result<String> {
    let mut turn_guard = crate::observability::AgentTurnGuard::start(
        turn.exec.observer,
        turn.exec.model_access.provider_name,
        turn.exec.model_access.model,
        Some(turn.channel_name.to_string()),
        turn.agent_alias.map(str::to_string),
        Some(turn.turn_id.to_string()),
    );
    let result = run_tool_call_loop_with_security(turn, security).await;
    // Snapshot token usage from the task-local cost context when the caller
    // scoped one around this call (the gateway scopes both
    // `TOOL_LOOP_TURN_USAGE` and `TOOL_LOOP_COST_TRACKING_CONTEXT` around
    // `process_message`); unscoped callers report `None`. When this runs
    // nested inside a parent turn's scoped context (peer-message-as-tool),
    // `snapshot_turn_usage` prefers the caller-scoped task-local and may
    // report the parent turn's cumulative usage — pre-existing cost
    // attribution semantics, kept as-is.
    let tokens_used = TOOL_LOOP_COST_TRACKING_CONTEXT
        .try_with(std::clone::Clone::clone)
        .ok()
        .flatten()
        .and_then(|ctx| {
            let usage = ctx.snapshot_turn_usage();
            (usage.input_tokens > 0 || usage.output_tokens > 0).then_some(
                reiclaw_api::observability_traits::TurnTokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                },
            )
        });
    turn_guard.set_usage(tokens_used, None);
    turn_guard.finish();
    result
}
