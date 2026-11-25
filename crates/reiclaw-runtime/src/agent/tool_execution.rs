//! Tool execution helpers extracted from `loop_`.

use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::approval::ApprovalManager;
use crate::observability::{Observer, ObserverEvent};
use crate::tools::{ActivatedToolSet, Tool};
use reiclaw_api::agent::TurnEvent;
use tokio::sync::mpsc::Sender;

// Items that still live in `loop_` — import via the parent module.
use super::loop_::{ParsedToolCall, ToolLoopCancelled, is_tool_loop_cancelled, scrub_credentials};
use super::turn::TurnMeta;
use super::turn::redact::scrub_credentials_json;
use super::turn::redact::scrub_credentials_value;
use super::turn::tool_logging::tool_log_attributes;

#[cfg(test)]
#[path = "mcp_recovery_tests.rs"]
mod mcp_recovery_tests;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Look up a tool by name in a slice of boxed `dyn Tool` values.
pub fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
}

enum ResolvedTool<'a> {
    Static(&'a dyn Tool),
    Activated(Arc<dyn Tool>),
}

impl ResolvedTool<'_> {
    fn tool(&self) -> &dyn Tool {
        match self {
            Self::Static(tool) => *tool,
            Self::Activated(tool) => tool.as_ref(),
        }
    }
}

fn resolve_tool<'a>(
    dispatch: ToolDispatchContext<'a>,
    name: &str,
    tool_call_id: Option<&str>,
) -> Option<ResolvedTool<'a>> {
    if let Some(tool) = find_tool(dispatch.tools_registry, name) {
        return Some(ResolvedTool::Static(tool));
    }
    if crate::tools::is_retired_operator_tool_name(name) {
        return None;
    }
    let activated = dispatch.activated_tools?;
    let activated = match activated.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_category(::reiclaw_log::EventCategory::Tool)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"tool": name, "tool_call_id": tool_call_id})),
                "activated-tool lock poisoned while resolving tool; recovering guard for read"
            );
            poisoned.into_inner()
        }
    };
    activated.get_resolved(name).map(ResolvedTool::Activated)
}

#[derive(Clone, Copy)]
pub(crate) struct ToolDispatchContext<'a> {
    pub tools_registry: &'a [Box<dyn Tool>],
    pub activated_tools: Option<&'a std::sync::Arc<std::sync::Mutex<ActivatedToolSet>>>,
    pub excluded_tools: &'a [String],
}

fn is_excluded_tool(name: &str, excluded_tools: &[String]) -> bool {
    let name = name.trim();
    excluded_tools
        .iter()
        .any(|excluded| excluded.trim().eq_ignore_ascii_case(name))
}

/// Admission uses the executor's canonical resolver. Only a completed
/// dispatch below supplies evidence to the per-turn repetition guard.
pub(crate) fn dispatch_tool_name(name: &str, dispatch: ToolDispatchContext<'_>) -> Option<String> {
    if is_excluded_tool(name, dispatch.excluded_tools) {
        return None;
    }
    let resolved = resolve_tool(dispatch, name, None)?;
    let name = resolved.tool().name();
    (!is_excluded_tool(name, dispatch.excluded_tools)).then(|| name.to_owned())
}

/// Answer only calls that cannot dispatch, without activating a deferred tool
/// or invoking its hooks. Preparation uses this before asking for approval.
/// A preceding tool_search can make a suffix alias resolvable in the same
/// batch, so unknown names remain undecided in that case. Exclusions still win.
pub(crate) fn undispatchable_tool_outcome(
    call_name: &str,
    call_arguments: &serde_json::Value,
    tool_call_id: Option<&str>,
    dispatch: ToolDispatchContext<'_>,
    may_activate_in_batch: bool,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
) -> Option<ToolExecutionOutcome> {
    let unavailable = if is_excluded_tool(call_name, dispatch.excluded_tools) {
        true
    } else if let Some(resolved) = resolve_tool(dispatch, call_name, tool_call_id) {
        if !is_excluded_tool(resolved.tool().name(), dispatch.excluded_tools) {
            return None;
        }
        true
    } else if crate::tools::is_retired_operator_tool_name(call_name) {
        false
    } else {
        let recoverable = dispatch.activated_tools.is_some_and(|activated| {
            let guard = activated
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.deferred_tool_names().any(|name| name == call_name)
        });
        if recoverable || may_activate_in_batch {
            return None;
        }
        false
    };

    let full_args = scrub_credentials_json(call_arguments);
    let tool_call_id_owned = tool_call_id.map(str::to_string);
    observer.record_event(&ObserverEvent::ToolCallStart {
        tool: call_name.to_string(),
        tool_call_id: tool_call_id_owned.clone(),
        arguments: Some(full_args.clone()),
        channel: Some(meta.channel_name.to_string()),
        agent_alias: meta.agent_alias.map(str::to_string),
        parent_agent_alias: meta.parent_agent_alias.map(str::to_string),
        turn_id: Some(meta.turn_id.to_string()),
    });
    Some(if unavailable {
        unavailable_tool_outcome(
            call_name,
            tool_call_id_owned,
            &full_args,
            meta,
            observer,
            Duration::ZERO,
        )
    } else {
        unknown_tool_outcome(
            call_name,
            tool_call_id_owned,
            &full_args,
            meta,
            observer,
            Duration::ZERO,
        )
    })
}

fn unknown_tool_outcome(
    call_name: &str,
    tool_call_id_owned: Option<String>,
    full_args: &str,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    duration: Duration,
) -> ToolExecutionOutcome {
    let reason = format!("Unknown tool: {call_name}");
    observer.record_event(&ObserverEvent::ToolCall {
        tool: call_name.to_string(),
        tool_call_id: tool_call_id_owned,
        duration,
        success: false,
        arguments: Some(full_args.to_string()),
        result: Some(scrub_credentials(&reason)),
        channel: Some(meta.channel_name.to_string()),
        agent_alias: meta.agent_alias.map(str::to_string),
        parent_agent_alias: meta.parent_agent_alias.map(str::to_string),
        turn_id: Some(meta.turn_id.to_string()),
    });
    ToolExecutionOutcome {
        output: reason.clone(),
        success: false,
        error_reason: Some(reason),
        duration,
        receipt: None,
    }
}

fn unavailable_tool_outcome(
    call_name: &str,
    tool_call_id_owned: Option<String>,
    full_args: &str,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    duration: Duration,
) -> ToolExecutionOutcome {
    let reason = format!("Tool not available in this turn: {call_name}");
    observer.record_event(&ObserverEvent::ToolCall {
        tool: call_name.to_string(),
        tool_call_id: tool_call_id_owned,
        duration,
        success: false,
        arguments: Some(full_args.to_string()),
        result: Some(scrub_credentials(&reason)),
        channel: Some(meta.channel_name.to_string()),
        agent_alias: meta.agent_alias.map(|s| s.to_string()),
        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
        turn_id: Some(meta.turn_id.to_string()),
    });
    ToolExecutionOutcome {
        output: reason.clone(),
        success: false,
        error_reason: Some(reason),
        duration,
        receipt: None,
    }
}

// ── Outcome ──────────────────────────────────────────────────────────────

pub struct ToolExecutionOutcome {
    pub output: String,
    pub success: bool,
    /// Raw failure text on the data path. Credential scrubbing is a rendering
    /// concern applied at each human-facing surface (observer events,
    /// post-execution log line, CLI progress), never stored pre-scrubbed here.
    pub error_reason: Option<String>,
    pub duration: Duration,
    /// Cryptographic HMAC receipt proving this tool actually executed.
    /// Present only when tool receipts are enabled in config.
    pub receipt: Option<String>,
}

// ── Single tool execution ────────────────────────────────────────────────

pub(crate) struct CompletedToolExecution {
    pub(crate) outcome: ToolExecutionOutcome,
    /// Present only after `Tool::execute` completes, using the resolved tool
    /// identity. Denied, unknown, unavailable and cancelled calls have no
    /// completed-execution evidence.
    pub(crate) executed_tool_name: Option<String>,
}

#[cfg(test)]
pub(crate) async fn execute_one_tool(
    call_name: &str,
    call_arguments: serde_json::Value,
    tool_call_id: Option<&str>,
    dispatch: ToolDispatchContext<'_>,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    receipt_generator: Option<&super::tool_receipts::ReceiptGenerator>,
    event_tx: Option<&Sender<TurnEvent>>,
) -> Result<ToolExecutionOutcome> {
    execute_one_tool_with_provenance(
        call_name,
        call_arguments,
        tool_call_id,
        dispatch,
        meta,
        observer,
        cancellation_token,
        receipt_generator,
        event_tx,
    )
    .await
    .map(|completed| completed.outcome)
}

async fn execute_one_tool_with_provenance(
    call_name: &str,
    call_arguments: serde_json::Value,
    tool_call_id: Option<&str>,
    dispatch: ToolDispatchContext<'_>,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    receipt_generator: Option<&super::tool_receipts::ReceiptGenerator>,
    event_tx: Option<&Sender<TurnEvent>>,
) -> Result<CompletedToolExecution> {
    // Rendering-boundary copy of the arguments for observer events. Results
    // are already scrubbed at this boundary; arguments carry the same
    // credential exposure (e.g. a rejected raw `api_key`) and get the same
    // treatment — via the structured walk so composite shapes the text regex
    // cannot see are redacted too. The data path below (`tool.execute`,
    // receipts) stays raw.
    let full_args = scrub_credentials_json(&call_arguments);
    let tool_call_id_owned = tool_call_id.map(str::to_string);
    observer.record_event(&ObserverEvent::ToolCallStart {
        tool: call_name.to_string(),
        tool_call_id: tool_call_id_owned.clone(),
        arguments: Some(full_args.clone()),
        channel: Some(meta.channel_name.to_string()),
        agent_alias: meta.agent_alias.map(|s| s.to_string()),
        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
        turn_id: Some(meta.turn_id.to_string()),
    });
    let start = Instant::now();

    if let Some(estop) = super::turn::execution::current_turn_estop() {
        estop.enforce_not_killed().map_err(|msg| {
            anyhow::Error::new(super::turn::provider_call::EstopAbortedError(msg))
        })?;
        if let Err(reason) = estop.enforce_tool(call_name) {
            let outcome = ToolExecutionOutcome {
                output: reason.clone(),
                success: false,
                error_reason: Some(reason),
                duration: start.elapsed(),
                receipt: None,
            };
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                tool_call_id: tool_call_id_owned,
                duration: outcome.duration,
                success: false,
                arguments: Some(full_args),
                result: Some(scrub_credentials(&outcome.output)),
                channel: Some(meta.channel_name.to_string()),
                agent_alias: meta.agent_alias.map(str::to_string),
                parent_agent_alias: meta.parent_agent_alias.map(str::to_string),
                turn_id: Some(meta.turn_id.to_string()),
            });
            return Ok(CompletedToolExecution {
                executed_tool_name: None,
                outcome,
            });
        }
    }

    if is_excluded_tool(call_name, dispatch.excluded_tools) {
        return Ok(CompletedToolExecution {
            executed_tool_name: None,
            outcome: unavailable_tool_outcome(
                call_name,
                tool_call_id_owned,
                &full_args,
                meta,
                observer,
                start.elapsed(),
            ),
        });
    }

    let resolved = resolve_tool(dispatch, call_name, tool_call_id).or_else(|| {
        if crate::tools::is_retired_operator_tool_name(call_name) {
            return None;
        }
        // Only actual dispatch may hydrate an authorized exact deferred name.
        // Admission's shared canonical resolver stays read-only, and hooks and
        // approval have already completed before this execution boundary.
        let activated = dispatch.activated_tools?;
        ActivatedToolSet::activate_exact(activated, call_name, dispatch.excluded_tools)
            .map(ResolvedTool::Activated)
    });
    let Some(resolved) = resolved else {
        return Ok(CompletedToolExecution {
            executed_tool_name: None,
            outcome: unknown_tool_outcome(
                call_name,
                tool_call_id_owned,
                &full_args,
                meta,
                observer,
                start.elapsed(),
            ),
        });
    };
    let tool = resolved.tool();

    if let Some(estop) = super::turn::execution::current_turn_estop() {
        estop.enforce_not_killed().map_err(|msg| {
            anyhow::Error::new(super::turn::provider_call::EstopAbortedError(msg))
        })?;
        if let Err(reason) = estop.enforce_tool(tool.name()) {
            let outcome = ToolExecutionOutcome {
                output: reason.clone(),
                success: false,
                error_reason: Some(reason),
                duration: start.elapsed(),
                receipt: None,
            };
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                tool_call_id: tool_call_id_owned,
                duration: outcome.duration,
                success: false,
                arguments: Some(full_args),
                result: Some(scrub_credentials(&outcome.output)),
                channel: Some(meta.channel_name.to_string()),
                agent_alias: meta.agent_alias.map(str::to_string),
                parent_agent_alias: meta.parent_agent_alias.map(str::to_string),
                turn_id: Some(meta.turn_id.to_string()),
            });
            return Ok(CompletedToolExecution {
                executed_tool_name: None,
                outcome,
            });
        }
    }

    if is_excluded_tool(tool.name(), dispatch.excluded_tools) {
        return Ok(CompletedToolExecution {
            executed_tool_name: None,
            outcome: unavailable_tool_outcome(
                call_name,
                tool_call_id_owned,
                &full_args,
                meta,
                observer,
                start.elapsed(),
            ),
        });
    }

    use ::reiclaw_log::Instrument;
    let tool_span = ::reiclaw_log::info_span!(
        target: "zeroclaw_log_internal_scope",
        "zeroclaw_scope",
        tool = %call_name,
    );

    // Tool I/O is captured only at the log rendering boundary, under the
    // live policy. Raw values still reach execution, history and receipts.
    if ::reiclaw_log::debug_enabled() {
        let _start_guard = tool_span.clone().entered();
        ::reiclaw_log::record!(
            DEBUG,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Invoke)
                .with_category(::reiclaw_log::EventCategory::Tool)
                .with_attrs(tool_log_attributes(
                    tool.name(),
                    ::serde_json::json!({
                        "tool": call_name,
                        "tool_call_id": tool_call_id,
                        "input": call_arguments,
                    })
                )),
            format!("tool call: {call_name}")
        );
    }

    // Stable correlation id for this call's pending ToolCall and terminal
    // ToolResult. Native calls carry their own id; id-less text-protocol calls
    // get one synthesized UUID reused for both halves so ACP/WS clients key the
    // tool_call_update to the right pending tool_call.
    let event_call_id = tool_call_id_owned
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    if let Some(tx) = event_tx {
        let _ = tx
            .send(TurnEvent::ToolCall {
                id: event_call_id.clone(),
                name: call_name.to_string(),
                // UI-facing turn event: same rendering-boundary scrub as the
                // observer events above. The raw args still reach the tool.
                args: scrub_credentials_value(call_arguments.clone()),
            })
            .await;
    }

    let estop = super::turn::execution::current_turn_estop();
    let resolved_tool_name = tool.name().to_string();
    let tool_future = tool
        .execute(call_arguments.clone())
        .instrument(tool_span.clone());
    tokio::pin!(tool_future);
    let execute = async {
        loop {
            if let Some(ref handle) = estop {
                handle.enforce_not_killed().map_err(|msg| {
                    anyhow::Error::new(super::turn::provider_call::EstopAbortedError(msg))
                })?;
                if let Err(reason) = handle
                    .enforce_tool(call_name)
                    .and_then(|()| handle.enforce_tool(&resolved_tool_name))
                {
                    return Ok(Ok(reiclaw_api::tool::ToolResult {
                        success: false,
                        output: reiclaw_api::tool::ToolOutput::default(),
                        error: Some(reason),
                    }));
                }
            }

            if estop.is_some() {
                let poll = tokio::time::sleep(Duration::from_millis(100));
                if let Some(token) = cancellation_token {
                    tokio::select! {
                        biased;
                        () = token.cancelled() => return Err::<_, anyhow::Error>(ToolLoopCancelled.into()),
                        result = &mut tool_future => return Ok(result),
                        _ = poll => continue,
                    }
                } else {
                    tokio::select! {
                        biased;
                        result = &mut tool_future => return Ok(result),
                        _ = poll => continue,
                    }
                }
            } else if let Some(token) = cancellation_token {
                tokio::select! {
                    () = token.cancelled() => return Err::<_, anyhow::Error>(ToolLoopCancelled.into()),
                    result = &mut tool_future => return Ok(result),
                }
            } else {
                return Ok((&mut tool_future).await);
            }
        }
    };
    let tool_result = execute.await?;

    let outcome = {
        let _result_guard = tool_span.entered();
        match tool_result {
            Ok(r) => {
                let duration = start.elapsed();
                if r.success && ::reiclaw_log::debug_enabled() {
                    ::reiclaw_log::record!(
                        DEBUG,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Complete)
                            .with_category(::reiclaw_log::EventCategory::Tool)
                            .with_outcome(::reiclaw_log::EventOutcome::Success)
                            .with_duration(duration.as_millis() as u64)
                            .with_attrs(tool_log_attributes(
                                tool.name(),
                                ::serde_json::json!({
                                    "tool": call_name,
                                    "tool_call_id": tool_call_id,
                                    "input": call_arguments,
                                    "output": r.output,
                                })
                            )),
                        format!("tool result: {call_name}")
                    );
                } else if !r.success {
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                            .with_category(::reiclaw_log::EventCategory::Tool)
                            .with_outcome(::reiclaw_log::EventOutcome::Failure)
                            .with_duration(duration.as_millis() as u64)
                            .with_attrs(tool_log_attributes(
                                tool.name(),
                                ::serde_json::json!({
                                    "tool": call_name,
                                    "tool_call_id": tool_call_id,
                                    "input": call_arguments,
                                    "error": r.error.clone().unwrap_or_default(),
                                    "output": r.output,
                                })
                            )),
                        format!("tool failed: {call_name}")
                    );
                }
                if r.success {
                    let normalized_output = if r.output.is_empty() {
                        "(no output)"
                    } else {
                        &r.output
                    };
                    let receipt = receipt_generator.map(|receipt_gen| {
                        receipt_gen.generate_now(call_name, &call_arguments, normalized_output)
                    });
                    observer.record_event(&ObserverEvent::ToolCall {
                        tool: call_name.to_string(),
                        tool_call_id: tool_call_id_owned.clone(),
                        duration,
                        success: true,
                        arguments: Some(full_args.clone()),
                        result: Some(scrub_credentials(normalized_output)),
                        channel: Some(meta.channel_name.to_string()),
                        agent_alias: meta.agent_alias.map(|s| s.to_string()),
                        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
                        turn_id: Some(meta.turn_id.to_string()),
                    });
                    Ok(ToolExecutionOutcome {
                        output: normalized_output.to_string(),
                        success: true,
                        error_reason: None,
                        duration,
                        receipt,
                    })
                } else {
                    let reason = r.error.unwrap_or_else(|| r.output.into_string());
                    observer.record_event(&ObserverEvent::ToolCall {
                        tool: call_name.to_string(),
                        tool_call_id: tool_call_id_owned.clone(),
                        duration,
                        success: false,
                        arguments: Some(full_args.clone()),
                        result: Some(scrub_credentials(&reason)),
                        channel: Some(meta.channel_name.to_string()),
                        agent_alias: meta.agent_alias.map(|s| s.to_string()),
                        parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
                        turn_id: Some(meta.turn_id.to_string()),
                    });
                    Ok(ToolExecutionOutcome {
                        output: format!("Error: {reason}"),
                        success: false,
                        error_reason: Some(reason),
                        duration,
                        receipt: None,
                    })
                }
            }
            Err(e) => {
                let duration = start.elapsed();
                ::reiclaw_log::record!(
                    ERROR,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                        .with_category(::reiclaw_log::EventCategory::Tool)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_duration(duration.as_millis() as u64)
                        .with_attrs(tool_log_attributes(
                            tool.name(),
                            ::serde_json::json!({
                                "tool": call_name,
                                "tool_call_id": tool_call_id,
                                "input": call_arguments,
                                "error": format!("{e:?}"),
                            })
                        )),
                    format!("tool error: {call_name}")
                );
                let reason = format!("Error executing {}: {e}", tool.name());
                observer.record_event(&ObserverEvent::ToolCall {
                    tool: call_name.to_string(),
                    tool_call_id: tool_call_id_owned.clone(),
                    duration,
                    success: false,
                    arguments: Some(full_args.clone()),
                    result: Some(scrub_credentials(&reason)),
                    channel: Some(meta.channel_name.to_string()),
                    agent_alias: meta.agent_alias.map(|s| s.to_string()),
                    parent_agent_alias: meta.parent_agent_alias.map(|s| s.to_string()),
                    turn_id: Some(meta.turn_id.to_string()),
                });
                Ok(ToolExecutionOutcome {
                    output: reason.clone(),
                    success: false,
                    error_reason: Some(reason),
                    duration,
                    receipt: None,
                })
            }
        }
    };

    if let Some(tx) = event_tx
        && let Ok(out) = &outcome
    {
        let _ = tx
            .send(TurnEvent::ToolResult {
                id: event_call_id.clone(),
                name: call_name.to_string(),
                output: scrub_credentials(&out.output),
            })
            .await;
    }

    outcome.map(|outcome| CompletedToolExecution {
        outcome,
        executed_tool_name: Some(tool.name().to_owned()),
    })
}

// ── Parallel / sequential decision ───────────────────────────────────────

pub fn should_execute_tools_in_parallel(
    tool_calls: &[ParsedToolCall],
    approval: Option<&ApprovalManager>,
) -> bool {
    if tool_calls.len() <= 1 {
        return false;
    }

    // tool_search activates deferred MCP tools into ActivatedToolSet.
    // Running tool_search in parallel with the tools it activates causes a
    // race condition where the tool lookup happens before activation completes.
    // Force sequential execution whenever tool_search is in the batch.
    if tool_calls.iter().any(|call| call.name == "tool_search") {
        return false;
    }

    if let Some(mgr) = approval
        && tool_calls.iter().any(|call| mgr.needs_approval(&call.name))
    {
        // Approval-gated calls must keep sequential handling so the caller can
        // enforce CLI prompt/deny policy consistently.
        return false;
    }

    true
}

// ── Parallel execution ───────────────────────────────────────────────────

pub(crate) async fn execute_tools_parallel(
    tool_calls: &[ParsedToolCall],
    dispatch: ToolDispatchContext<'_>,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    receipt_generator: Option<&super::tool_receipts::ReceiptGenerator>,
    event_tx: Option<&Sender<TurnEvent>>,
) -> Result<Vec<Option<CompletedToolExecution>>> {
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|call| {
            execute_one_tool_with_provenance(
                &call.name,
                call.arguments.clone(),
                call.tool_call_id.as_deref(),
                dispatch,
                meta,
                observer,
                cancellation_token,
                receipt_generator,
                event_tx,
            )
        })
        .collect();

    let results = futures_util::future::join_all(futures).await;
    let mut slots = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok(outcome) => slots.push(Some(outcome)),
            Err(e) if is_tool_loop_cancelled(&e) => slots.push(None),
            Err(e) => return Err(e),
        }
    }
    Ok(slots)
}

// ── Sequential execution ─────────────────────────────────────────────────

pub(crate) async fn execute_tools_sequential(
    tool_calls: &[ParsedToolCall],
    dispatch: ToolDispatchContext<'_>,
    meta: &TurnMeta<'_>,
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    receipt_generator: Option<&super::tool_receipts::ReceiptGenerator>,
    event_tx: Option<&Sender<TurnEvent>>,
) -> Result<Vec<Option<CompletedToolExecution>>> {
    let mut slots: Vec<Option<CompletedToolExecution>> = Vec::with_capacity(tool_calls.len());

    for call in tool_calls {
        if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            break;
        }
        let outcome = match execute_one_tool_with_provenance(
            &call.name,
            call.arguments.clone(),
            call.tool_call_id.as_deref(),
            dispatch,
            meta,
            observer,
            cancellation_token,
            receipt_generator,
            event_tx,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) if is_tool_loop_cancelled(&e) => break,
            Err(e) => return Err(e),
        };
        slots.push(Some(outcome));
    }

    slots.resize_with(tool_calls.len(), || None);
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use super::{ToolDispatchContext, execute_one_tool};
    use crate::observability::noop::NoopObserver;
    use crate::tools::ActivatedToolSet;
    use async_trait::async_trait;
    use reiclaw_api::tool::Tool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Minimal tool that records invocations. Used to verify that the
    /// poisoned-lock recovery path still resolves an activated tool and
    /// calls its execute method successfully.
    struct CountingTool {
        name: String,
        invocations: Arc<AtomicUsize>,
    }

    impl CountingTool {
        fn new(name: &str, invocations: Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                invocations,
            }
        }
    }

    impl reiclaw_api::attribution::Attributable for CountingTool {
        fn role(&self) -> reiclaw_api::attribution::Role {
            reiclaw_api::attribution::Role::System
        }
        fn alias(&self) -> &str {
            "test-counting-tool"
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Counts executions for poisoned-lock tests"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            })
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "executed via poisoned lock recovery".into(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn execute_one_tool_recovers_poisoned_activated_tool_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let invocations = Arc::new(AtomicUsize::new(0));
        let activated_tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
            "docker-mcp__extract_text",
            Arc::clone(&invocations),
        ));
        activated
            .lock()
            .unwrap()
            .activate("docker-mcp__extract_text".into(), activated_tool);

        // Poison the mutex by panicking while holding the lock in a
        // separate thread.
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("deliberately poison the activated-tools lock");
        })
        .join();

        // execute_one_tool must recover the poisoned lock and resolve
        // the activated tool without panicking.
        let meta = crate::agent::turn::TurnMeta {
            parent_agent_alias: None,
            agent_alias: None,
            turn_id: "test-turn-id",
            channel_name: "test",
        };
        let outcome = execute_one_tool(
            "docker-mcp__extract_text",
            serde_json::json!({}),
            None,
            ToolDispatchContext {
                tools_registry: &[], // no static tools - force activated-tools path
                activated_tools: Some(&activated),
                excluded_tools: &[],
            },
            &meta,
            &NoopObserver,
            None,
            None,
            None,
        )
        .await
        .expect("execute_one_tool should recover from poisoned lock");

        assert!(
            outcome.success,
            "activated tool execution should succeed after poisoned lock recovery"
        );
        assert!(
            outcome
                .output
                .contains("executed via poisoned lock recovery"),
            "tool output should come from the recovered activated tool"
        );
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            1,
            "recovered activated tool should have been invoked exactly once"
        );
    }

    #[tokio::test]
    async fn execute_one_tool_blocks_excluded_activated_suffix_resolution() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let invocations = Arc::new(AtomicUsize::new(0));
        let activated_tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
            "docker-mcp__extract_text",
            Arc::clone(&invocations),
        ));
        activated
            .lock()
            .unwrap()
            .activate("docker-mcp__extract_text".into(), activated_tool);

        let meta = crate::agent::turn::TurnMeta {
            parent_agent_alias: None,
            agent_alias: None,
            turn_id: "test-turn-id",
            channel_name: "test",
        };
        let excluded = vec!["docker-mcp__extract_text".to_string()];
        let outcome = execute_one_tool(
            "extract_text",
            serde_json::json!({}),
            Some("call-1"),
            ToolDispatchContext {
                tools_registry: &[],
                activated_tools: Some(&activated),
                excluded_tools: &excluded,
            },
            &meta,
            &NoopObserver,
            None,
            None,
            None,
        )
        .await
        .expect("excluded activated tool should return an unavailable outcome");

        assert!(!outcome.success);
        assert_eq!(
            outcome.output,
            "Tool not available in this turn: extract_text"
        );
        assert_eq!(invocations.load(Ordering::SeqCst), 0);
    }

    /// Tool that captures the exact arguments it received into a shared cell,
    /// so tests can distinguish the raw data path from the scrubbed rendering
    /// surface.
    struct ArgsCapturingTool {
        received: Arc<Mutex<Option<serde_json::Value>>>,
    }

    impl reiclaw_api::attribution::Attributable for ArgsCapturingTool {
        fn role(&self) -> reiclaw_api::attribution::Role {
            reiclaw_api::attribution::Role::System
        }
        fn alias(&self) -> &str {
            "test-args-capturing-tool"
        }
    }

    #[async_trait]
    impl Tool for ArgsCapturingTool {
        fn name(&self) -> &str {
            "args_capture"
        }

        fn description(&self) -> &str {
            "Captures received arguments for rendering-boundary tests"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}, "required": []})
        }

        async fn execute(
            &self,
            args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            *self.received.lock().unwrap() = Some(args);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "captured".into(),
                error: None,
            })
        }
    }

    /// Observer that records the arguments string of every tool lifecycle
    /// event, standing in for the log/UI surfaces that render them.
    struct RecordingObserver {
        tool_event_args: Mutex<Vec<String>>,
    }

    impl RecordingObserver {
        fn args_snapshots(&self) -> Vec<String> {
            self.tool_event_args.lock().unwrap().clone()
        }
    }

    impl crate::observability::Observer for RecordingObserver {
        fn record_event(&self, event: &crate::observability::ObserverEvent) {
            use crate::observability::ObserverEvent;
            let arguments = match event {
                ObserverEvent::ToolCallStart { arguments, .. } => arguments.clone(),
                ObserverEvent::ToolCall { arguments, .. } => arguments.clone(),
                _ => return,
            };
            if let Some(args) = arguments {
                self.tool_event_args.lock().unwrap().push(args);
            }
        }

        fn name(&self) -> &str {
            "test-recording-observer"
        }
    }

    #[tokio::test]
    async fn execute_one_tool_scrubs_credential_args_on_rendering_surfaces() {
        let received = Arc::new(Mutex::new(None));
        let tool = ArgsCapturingTool {
            received: Arc::clone(&received),
        };
        let observer = RecordingObserver {
            tool_event_args: Mutex::new(Vec::new()),
        };
        let meta = crate::agent::turn::TurnMeta {
            parent_agent_alias: None,
            agent_alias: None,
            turn_id: "test-turn-id",
            channel_name: "test",
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<reiclaw_api::agent::TurnEvent>(8);

        let secret_args = serde_json::json!({
            "action": "upsert_agent",
            "api_key": "sk-test-raw-secret-material",
            // Composite credential shape: the text regex cannot see a
            // non-scalar under a secret-named key; the structured walk must.
            "metadata": {"api_key": {"nested": "sk-composite-secret-value"}},
            "model": "gpt-5.3-codex"
        });
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(tool)];
        let outcome = execute_one_tool(
            "args_capture",
            secret_args.clone(),
            Some("call-1"),
            ToolDispatchContext {
                tools_registry: &registry,
                activated_tools: None,
                excluded_tools: &[],
            },
            &meta,
            &observer,
            None,
            None,
            Some(&tx),
        )
        .await
        .expect("tool should execute");

        assert!(outcome.success);

        // Data path: the tool itself received the raw, unmodified arguments.
        let received = received.lock().unwrap().clone();
        assert_eq!(
            received.as_ref(),
            Some(&secret_args),
            "scrubbing must stay a rendering concern; the tool gets raw args"
        );

        // Rendering path: observer lifecycle events carry a scrubbed copy.
        let snapshots = observer.args_snapshots();
        assert!(
            snapshots.len() >= 2,
            "expected ToolCallStart and ToolCall events, got: {snapshots:?}"
        );
        for rendered in &snapshots {
            assert!(
                !rendered.contains("sk-test-raw-secret-material"),
                "observer event must not echo the secret: {rendered}"
            );
            assert!(
                !rendered.contains("sk-composite-secret-value"),
                "observer event must not echo the composite-wrapped secret: {rendered}"
            );
            assert!(
                rendered.contains("[REDACTED]"),
                "observer event should show the redaction marker: {rendered}"
            );
        }

        // Rendering path: the UI-facing turn event carries a scrubbed copy.
        let mut turn_call_args: Option<serde_json::Value> = None;
        while let Ok(event) = rx.try_recv() {
            if let reiclaw_api::agent::TurnEvent::ToolCall { args, .. } = event {
                turn_call_args = Some(args);
            }
        }
        let args = turn_call_args.expect("TurnEvent::ToolCall must be emitted");
        let rendered = args.to_string();
        assert!(
            !rendered.contains("sk-test-raw-secret-material"),
            "turn event must not echo the secret: {rendered}"
        );
        assert!(
            !rendered.contains("sk-composite-secret-value"),
            "turn event must not echo the composite-wrapped secret: {rendered}"
        );
        assert!(
            rendered.contains("[REDACTED]"),
            "turn event should show the redaction marker: {rendered}"
        );
        assert_eq!(args["model"], serde_json::json!("gpt-5.3-codex"));
    }

    use super::should_execute_tools_in_parallel;
    use crate::agent::loop_::ParsedToolCall;
    use crate::approval::ApprovalManager;
    use reiclaw_config::autonomy::AutonomyLevel;
    use reiclaw_config::schema::RiskProfileConfig;

    fn parsed_tool_call(name: &str) -> ParsedToolCall {
        ParsedToolCall {
            name: name.to_string(),
            arguments: serde_json::json!({}),
            tool_call_id: None,
        }
    }

    fn supervised_risk_profile() -> RiskProfileConfig {
        RiskProfileConfig {
            level: AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".into()],
            always_ask: vec!["shell".into()],
            ..RiskProfileConfig::default()
        }
    }

    // --- tool_search branch---

    #[test]
    fn tool_search_in_batch_forces_serial() {
        // Two non-approval-gated tools in a batch where one is `tool_search`
        // must run sequentially. Without the `tool_search` branch the default
        // path would return `true` and the runtime would dispatch them in
        // parallel, racing the lookup against the activation.
        let calls = vec![
            parsed_tool_call("tool_search"),
            parsed_tool_call("file_read"),
        ];

        assert!(
            !should_execute_tools_in_parallel(&calls, None),
            "batch containing tool_search must force sequential execution (line 349-351)"
        );
    }

    #[test]
    fn tool_search_with_approval_required_in_batch_still_forces_serial() {
        // When both branches would trigger, the test only needs to confirm
        // the call still returns `false` — the ordering between the
        // `tool_search` branch and the approval branch is an implementation
        // detail. The important invariant is: `tool_search` present ⇒ serial.
        let calls = vec![parsed_tool_call("tool_search"), parsed_tool_call("shell")];
        let approval_cfg = reiclaw_config::schema::RiskProfileConfig::default();
        let approval_mgr = ApprovalManager::from_risk_profile(&approval_cfg);

        assert!(
            !should_execute_tools_in_parallel(&calls, Some(&approval_mgr)),
            "tool_search in a mixed approval batch must still force sequential execution"
        );
    }

    #[test]
    fn non_search_non_approval_batch_remains_parallel_eligible() {
        let calls = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("memory_recall"),
        ];

        assert!(
            should_execute_tools_in_parallel(&calls, None),
            "non-tool_search, non-approval batch must remain parallel-eligible (default branch)"
        );
    }

    // --- approval-required + control branches---

    #[test]
    fn approval_required_batch_forces_sequential() {
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("shell"),
            parsed_tool_call("file_read"),
        ];
        assert!(
            !should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "batch with approval-required tool must execute sequentially"
        );
    }

    #[test]
    fn approval_required_alone_in_batch_still_sequential() {
        // A two-element batch where one tool requires approval must still
        // take the serial branch (length check above already returns false
        // for len <= 1; this asserts the approval branch is the actual gate).
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![parsed_tool_call("file_read"), parsed_tool_call("shell")];
        assert!(
            !should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "approval branch must trigger regardless of approval tool position"
        );
    }

    #[test]
    fn mixed_batch_with_approval_forces_serial_even_with_parallel_candidates() {
        // Mixed batch: two file_read (parallel candidates) plus one shell
        // (approval-required). The presence of `shell` must force serial
        // execution, even though the other two could otherwise run in
        // parallel.
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("shell"),
            parsed_tool_call("file_read"),
        ];
        assert!(
            !should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "mixed batch must serialize when any approval-required tool is present"
        );
    }

    #[test]
    fn parallel_when_no_approval_and_no_tool_search() {
        // Control case: a batch of three non-approval, non-tool_search
        // calls under `Supervised` (where `file_read` is auto-approved and
        // `shell` is approval-required) may run in parallel.
        let mgr = ApprovalManager::for_non_interactive(&supervised_risk_profile());
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("file_read"),
            parsed_tool_call("file_read"),
        ];
        assert!(
            should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "non-approval, non-tool_search batch must run in parallel when allowed"
        );
    }

    #[test]
    fn full_autonomy_batch_with_unknown_tool_runs_in_parallel() {
        // Under `Full` autonomy, no tool requires approval — `needs_approval`
        // returns false for every name. The control case extends to a batch
        // whose names would otherwise be unknown to supervised profile.
        let full = RiskProfileConfig {
            level: AutonomyLevel::Full,
            ..RiskProfileConfig::default()
        };
        let mgr = ApprovalManager::for_non_interactive(&full);
        let batch = vec![
            parsed_tool_call("file_write"),
            parsed_tool_call("shell"),
            parsed_tool_call("anything"),
        ];
        assert!(
            should_execute_tools_in_parallel(&batch, Some(&mgr)),
            "full autonomy never prompts, so parallel execution is allowed"
        );
    }

    #[test]
    fn no_approval_manager_with_multi_call_batch_runs_in_parallel() {
        // When the caller passes `None` for `approval` and no tool in the
        // batch is `tool_search`, the function takes the parallel branch
        // unconditionally — useful for the tests / harnesses that exercise
        // the tool loop without an approval manager.
        let batch = vec![
            parsed_tool_call("file_read"),
            parsed_tool_call("memory_recall"),
        ];
        assert!(
            should_execute_tools_in_parallel(&batch, None),
            "no approval manager + non-tool_search batch must run in parallel"
        );
    }

    #[tokio::test]
    async fn retired_bare_memory_tools_do_not_suffix_resolve_to_activated_mcp_tools() {
        use super::{dispatch_tool_name, undispatchable_tool_outcome};

        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let purge_invocations = Arc::new(AtomicUsize::new(0));
        let export_invocations = Arc::new(AtomicUsize::new(0));
        {
            let mut guard = activated.lock().unwrap();
            guard.activate(
                "archive__memory_purge".into(),
                Arc::new(CountingTool::new(
                    "archive__memory_purge",
                    Arc::clone(&purge_invocations),
                )),
            );
            guard.activate(
                "archive__memory_export".into(),
                Arc::new(CountingTool::new(
                    "archive__memory_export",
                    Arc::clone(&export_invocations),
                )),
            );
        }

        let meta = crate::agent::turn::TurnMeta {
            parent_agent_alias: None,
            agent_alias: None,
            turn_id: "test-turn-id",
            channel_name: "test",
        };
        let dispatch = ToolDispatchContext {
            tools_registry: &[],
            activated_tools: Some(&activated),
            excluded_tools: &[],
        };

        for bare in ["memory_purge", "memory_export", " MEMORY_PURGE "] {
            assert!(
                dispatch_tool_name(bare, dispatch).is_none(),
                "retired bare tool name {bare:?} must not resolve to an MCP suffix"
            );
            let preflight = undispatchable_tool_outcome(
                bare,
                &serde_json::json!({}),
                Some("call-preflight"),
                dispatch,
                true,
                &meta,
                &NoopObserver,
            )
            .expect("retired bare tool name must fail eagerly even if tool_search is in batch");
            assert!(!preflight.success);
            assert_eq!(preflight.output, format!("Unknown tool: {bare}"));

            let exec = execute_one_tool(
                bare,
                serde_json::json!({}),
                Some("call-exec"),
                dispatch,
                &meta,
                &NoopObserver,
                None,
                None,
                None,
            )
            .await
            .expect("retired bare tool name must return Unknown tool outcome");
            assert!(!exec.success);
            assert_eq!(exec.output, format!("Unknown tool: {bare}"));
        }

        assert_eq!(purge_invocations.load(Ordering::SeqCst), 0);
        assert_eq!(export_invocations.load(Ordering::SeqCst), 0);

        for qualified in ["archive__memory_purge", "archive__memory_export"] {
            assert_eq!(
                dispatch_tool_name(qualified, dispatch).as_deref(),
                Some(qualified)
            );
            assert!(
                undispatchable_tool_outcome(
                    qualified,
                    &serde_json::json!({}),
                    Some("call-ok-preflight"),
                    dispatch,
                    true,
                    &meta,
                    &NoopObserver,
                )
                .is_none()
            );
            let exec = execute_one_tool(
                qualified,
                serde_json::json!({}),
                Some("call-ok"),
                dispatch,
                &meta,
                &NoopObserver,
                None,
                None,
                None,
            )
            .await
            .expect("qualified MCP tool must execute normally");
            assert!(exec.success);
            assert!(exec.output.contains("executed via poisoned lock recovery"));
        }

        assert_eq!(purge_invocations.load(Ordering::SeqCst), 1);
        assert_eq!(export_invocations.load(Ordering::SeqCst), 1);
    }
}
