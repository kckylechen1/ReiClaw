//! Request and event formats shared by non-streaming and streaming Responses calls.
use crate::multimodal;
use crate::traits::{
    ChatMessage, StreamChunk, StreamError, StreamEvent, StreamResult, TokenUsage,
    ToolCall as ProviderToolCall,
};
use reiclaw_api::tool::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const DEFAULT_INSTRUCTIONS: &str = "You are ReiClaw, a concise and helpful coding assistant.";
// A marker in existing persisted tool-call history.
const RESPONSES_HISTORY_PROVIDER: &str = "openai_codex";
const RESPONSES_HISTORY_KIND: &str = "responses_output_items";

// Keep the parser's memory bounded independently of the upstream token limits.
const MAX_SSE_FRAME_BYTES: usize = 1_048_576;
const MAX_STREAM_TOOL_CALLS: usize = 128;
const MAX_STREAM_TOOL_BYTES: usize = 4 * 1_048_576;

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesToolSpec {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) description: String,
    /// `Arc`-shared with the tool registry's stored schema — serialized
    /// transparently, never deep-cloned per request
    pub(crate) parameters: std::sync::Arc<Value>,
    pub(crate) strict: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesResponse {
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<Value>,
}

#[derive(Debug, Default)]
pub(crate) struct ResponsesStreamState {
    pub(crate) saw_text_delta: bool,
    pub(crate) saw_completion: bool,
    pub(crate) fallback_text: Option<String>,
    pub(crate) tool_calls: HashMap<String, PendingToolCall>,
    emitted_tool_call_ids: HashSet<String>,
    tool_bytes: usize,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct PendingToolCall {
    pub(crate) item_id: Option<String>,
    pub(crate) call_id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) arguments: String,
}

pub(crate) fn first_nonempty(text: Option<&str>) -> Option<String> {
    text.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(crate) fn convert_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<ResponsesToolSpec>> {
    let items = tools?;
    if items.is_empty() {
        return None;
    }

    Some(
        items
            .iter()
            .map(|tool| ResponsesToolSpec {
                kind: "function".to_string(),
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: std::sync::Arc::clone(&tool.parameters),
                strict: false,
            })
            .collect(),
    )
}

fn response_message_item(role: &str, content: Vec<Value>) -> Value {
    serde_json::json!({
        "type": "message",
        "role": role,
        "content": content,
    })
}

fn legacy_tool_output_message(content: &str) -> Value {
    response_message_item(
        "user",
        vec![serde_json::json!({
            "type": "input_text",
            "text": format!("Legacy tool output without call_id:\n{content}"),
        })],
    )
}

fn response_item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

fn is_replayable_responses_output_item(item: &Value) -> bool {
    matches!(
        response_item_type(item),
        Some("message" | "reasoning" | "function_call")
    )
}

fn decode_responses_history_items(reasoning_content: &str) -> Option<Vec<Value>> {
    let value = serde_json::from_str::<Value>(reasoning_content).ok()?;
    if value.get("provider").and_then(Value::as_str) != Some(RESPONSES_HISTORY_PROVIDER)
        || value.get("kind").and_then(Value::as_str) != Some(RESPONSES_HISTORY_KIND)
    {
        return None;
    }

    let items = value
        .get("items")
        .and_then(Value::as_array)?
        .iter()
        .filter(|item| is_replayable_responses_output_item(item))
        .cloned()
        .collect::<Vec<_>>();

    (!items.is_empty()).then_some(items)
}

/// Build a single Responses-API `function_call` input item from a parsed
/// `ProviderToolCall`. The `arguments` field is routed through the shared
/// `sanitize_tool_arguments` helper so the openai_codex call site inherits
/// the same malformed-JSON → `"{}"` contract as the other four typed
/// providers. Factored out so the call-site behavior is testable without
/// running the full input builder.
// Never include raw tool arguments in diagnostics.
fn sanitize_tool_arguments(function_name: &str, arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return "{}".to_string();
    }
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(serde_json::Value::Object(_)) => return arguments.to_string(),
        Ok(_non_object) => {
            // Accept only JSON objects; null, arrays, strings, numbers, and
            // booleans do not satisfy a strict-provider function-arguments
            // contract (reported by Cohere, tracked by OpenRouter's
            // auto-exacto validator).
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "function": function_name,
                        "payload_len": arguments.len(),
                        "error_key": "tool_args_not_object",
                    })),
                "Non-object tool-call arguments being sent to strict upstream provider, dropping to empty object"
            );
            return "{}".to_string();
        }
        Err(_) => {}
    }
    ::reiclaw_log::record!(
        WARN,
        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
            .with_outcome(::reiclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "function": function_name,
                "payload_len": arguments.len(),
                "error_key": "tool_args_invalid_json",
            })),
        "Invalid JSON in tool-call arguments being sent to upstream provider, dropping to empty object"
    );
    "{}".to_string()
}

pub(crate) fn build_function_call_item(call: ProviderToolCall) -> Value {
    let name = call.name;
    serde_json::json!({
        "type": "function_call",
        "call_id": call.id,
        "name": name,
        "arguments": sanitize_tool_arguments(&name, &call.arguments),
    })
}

pub(crate) fn build_responses_input(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => system_parts.push(&msg.content),
            "user" => {
                let (cleaned_text, image_refs) = multimodal::parse_image_markers(&msg.content);

                let mut content_items = Vec::new();

                if !cleaned_text.trim().is_empty() {
                    content_items.push(serde_json::json!({
                        "type": "input_text",
                        "text": cleaned_text,
                    }));
                }

                for image_ref in image_refs {
                    content_items.push(serde_json::json!({
                        "type": "input_image",
                        "image_url": image_ref,
                    }));
                }

                if content_items.is_empty() {
                    content_items.push(serde_json::json!({
                        "type": "input_text",
                        "text": "",
                    }));
                }

                input.push(response_message_item("user", content_items));
            }
            "assistant" => {
                if let Ok(value) = serde_json::from_str::<Value>(&msg.content)
                    && let Some(tool_calls_value) = value.get("tool_calls")
                    && let Ok(parsed_calls) =
                        serde_json::from_value::<Vec<ProviderToolCall>>(tool_calls_value.clone())
                {
                    let content = value
                        .get("content")
                        .and_then(Value::as_str)
                        .filter(|content| !content.trim().is_empty());
                    let responses_history_items = value
                        .get("reasoning_content")
                        .and_then(Value::as_str)
                        .and_then(decode_responses_history_items);

                    if let Some(items) = responses_history_items {
                        if let Some(content) = content
                            && !items
                                .iter()
                                .any(|item| response_item_type(item) == Some("message"))
                        {
                            input.push(response_message_item(
                                "assistant",
                                vec![serde_json::json!({
                                    "type": "output_text",
                                    "text": content,
                                })],
                            ));
                        }

                        input.extend(items);
                        continue;
                    }

                    if let Some(content) = value
                        .get("content")
                        .and_then(Value::as_str)
                        .filter(|content| !content.trim().is_empty())
                    {
                        input.push(response_message_item(
                            "assistant",
                            vec![serde_json::json!({
                                "type": "output_text",
                                "text": content,
                            })],
                        ));
                    }

                    for call in parsed_calls {
                        input.push(build_function_call_item(call));
                    }
                } else if !msg.content.trim().is_empty() {
                    input.push(response_message_item(
                        "assistant",
                        vec![serde_json::json!({
                            "type": "output_text",
                            "text": msg.content,
                        })],
                    ));
                }
            }
            "tool" => {
                if let Ok(value) = serde_json::from_str::<Value>(&msg.content) {
                    if let Some(call_id) = value
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .and_then(|id| first_nonempty(Some(id)))
                    {
                        let output = value
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        input.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output,
                        }));
                    } else if !msg.content.trim().is_empty() {
                        input.push(legacy_tool_output_message(&msg.content));
                    }
                } else if !msg.content.trim().is_empty() {
                    input.push(legacy_tool_output_message(&msg.content));
                }
            }
            _ => {}
        }
    }

    let instructions = if system_parts.is_empty() {
        DEFAULT_INSTRUCTIONS.to_string()
    } else {
        system_parts.join("\n\n")
    };

    (instructions, input)
}

pub(crate) fn nonempty_preserve(text: Option<&str>) -> Option<String> {
    text.and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

pub(crate) fn extract_responses_text(body: &ResponsesResponse) -> Option<String> {
    if let Some(text) = first_nonempty(body.output_text.as_deref()) {
        return Some(text);
    }
    for item in &body.output {
        if item.get("type").and_then(serde_json::Value::as_str) != Some("message") {
            continue;
        }
        if let Some(parts) = item.get("content").and_then(serde_json::Value::as_array) {
            for part in parts {
                if part.get("type").and_then(serde_json::Value::as_str) == Some("output_text")
                    && let Some(text) =
                        first_nonempty(part.get("text").and_then(serde_json::Value::as_str))
                {
                    return Some(text);
                }
            }
        }
    }
    None
}

pub(crate) fn extract_responses_tool_calls(body: &ResponsesResponse) -> Vec<ProviderToolCall> {
    body.output
        .iter()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
        })
        .filter_map(|item| {
            let name = item
                .get("name")
                .and_then(serde_json::Value::as_str)?
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("{}")
                .to_string();
            let id = item
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.get("id").and_then(serde_json::Value::as_str))
                .map(ToString::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            Some(ProviderToolCall {
                id,
                name,
                arguments,
                extra_content: None,
            })
        })
        .collect()
}

pub(crate) fn parse_responses_usage(usage: Option<&Value>) -> Option<TokenUsage> {
    let usage = usage?.as_object()?;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
    let cached_input_tokens = usage
        .get("input_tokens_details")
        .and_then(Value::as_object)
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64);

    (input_tokens.is_some() || output_tokens.is_some() || cached_input_tokens.is_some()).then_some(
        TokenUsage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        },
    )
}

fn response_output_text_from_event_item(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }

    item.get("content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts.iter().find_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("output_text") {
                    first_nonempty(part.get("text").and_then(Value::as_str))
                } else {
                    None
                }
            })
        })
}

fn pending_tool_call_key(item_id: Option<&str>, output_index: Option<u64>) -> Option<String> {
    item_id
        .map(ToString::to_string)
        .or_else(|| output_index.map(|index| format!("output:{index}")))
}

impl ResponsesStreamState {
    fn pending_call(&mut self, key: String) -> anyhow::Result<&mut PendingToolCall> {
        if !self.tool_calls.contains_key(&key) {
            anyhow::ensure!(
                self.tool_calls.len() < MAX_STREAM_TOOL_CALLS,
                "SSE pending tool calls exceed {MAX_STREAM_TOOL_CALLS}"
            );
        }
        Ok(self.tool_calls.entry(key).or_default())
    }

    fn account_tool_bytes(&mut self, bytes: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            bytes <= MAX_STREAM_TOOL_BYTES.saturating_sub(self.tool_bytes),
            "SSE tool-call deltas exceed {MAX_STREAM_TOOL_BYTES} bytes"
        );
        self.tool_bytes += bytes;
        Ok(())
    }

    fn emit_tool_call(&mut self, call: ProviderToolCall) -> anyhow::Result<Option<StreamEvent>> {
        if self.emitted_tool_call_ids.contains(&call.id) {
            return Ok(None);
        }
        anyhow::ensure!(
            self.emitted_tool_call_ids.len() < MAX_STREAM_TOOL_CALLS,
            "SSE tool calls exceed {MAX_STREAM_TOOL_CALLS}"
        );
        self.account_tool_bytes(call.id.len())?;
        self.emitted_tool_call_ids.insert(call.id.clone());
        Ok(Some(StreamEvent::ToolCall(call)))
    }
}

pub(crate) fn process_responses_stream_event(
    event: Value,
    state: &mut ResponsesStreamState,
) -> anyhow::Result<Vec<StreamEvent>> {
    if let Some(message) = extract_stream_error_message(&event) {
        anyhow::bail!("OpenAI responses stream error: {message}");
    }

    let mut emitted = Vec::new();
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            if let Some(text) = nonempty_preserve(event.get("delta").and_then(Value::as_str)) {
                state.saw_text_delta = true;
                emitted.push(StreamEvent::TextDelta(StreamChunk::delta(text)));
            }
        }
        Some("response.output_text.done") if !state.saw_text_delta => {
            state.fallback_text = nonempty_preserve(event.get("text").and_then(Value::as_str));
        }
        Some("response.output_item.added") => {
            let item = event.get("item");
            let item_type = item
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str);
            if item_type == Some("function_call") {
                let key = pending_tool_call_key(
                    item.and_then(|value| value.get("id"))
                        .and_then(Value::as_str),
                    event.get("output_index").and_then(Value::as_u64),
                );
                if let Some(key) = key {
                    let arguments = item
                        .and_then(|value| value.get("arguments"))
                        .and_then(Value::as_str);
                    let metadata_bytes = ["id", "call_id", "name"]
                        .into_iter()
                        .filter_map(|field| item?.get(field)?.as_str())
                        .map(str::len)
                        .sum::<usize>();
                    state.account_tool_bytes(arguments.map_or(0, str::len) + metadata_bytes)?;
                    let entry = state.pending_call(key)?;
                    entry.item_id = item
                        .and_then(|value| value.get("id"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    entry.call_id = item
                        .and_then(|value| value.get("call_id"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    entry.name = item
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    if let Some(arguments) = arguments {
                        entry.arguments = arguments.to_string();
                    }
                }
            }
        }
        Some("response.function_call_arguments.delta") => {
            if let Some(key) = pending_tool_call_key(
                event.get("item_id").and_then(Value::as_str),
                event.get("output_index").and_then(Value::as_u64),
            ) {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                state.account_tool_bytes(delta.len() + key.len())?;
                let entry = state.pending_call(key)?;
                entry.item_id = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                entry.arguments.push_str(delta);
            }
        }
        Some("response.function_call_arguments.done") => {
            let key = pending_tool_call_key(
                event.get("item_id").and_then(Value::as_str),
                event.get("output_index").and_then(Value::as_u64),
            );
            let mut pending = key
                .as_ref()
                .and_then(|key| state.tool_calls.remove(key))
                .unwrap_or_default();
            pending.item_id = pending.item_id.or_else(|| {
                event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            });
            pending.call_id = pending.call_id.or_else(|| {
                event
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            });
            pending.name = pending.name.or_else(|| {
                event
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            });
            if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                pending.arguments = arguments.to_string();
            }

            if let Some(name) = pending.name {
                let tool_call = ProviderToolCall {
                    id: pending
                        .call_id
                        .or(pending.item_id)
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    name,
                    arguments: pending.arguments,
                    extra_content: None,
                };
                if let Some(event) = state.emit_tool_call(tool_call)? {
                    emitted.push(event);
                }
            }
        }
        Some("response.output_item.done") => {
            if let Some(item) = event.get("item") {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") if !state.saw_text_delta && state.fallback_text.is_none() => {
                        state.fallback_text = response_output_text_from_event_item(item);
                    }
                    Some("function_call") => {
                        if let Some(name) = item.get("name").and_then(Value::as_str) {
                            let tool_call = ProviderToolCall {
                                id: item
                                    .get("call_id")
                                    .and_then(Value::as_str)
                                    .or_else(|| item.get("id").and_then(Value::as_str))
                                    .unwrap_or_default()
                                    .to_string(),
                                name: name.to_string(),
                                arguments: item
                                    .get("arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                extra_content: None,
                            };
                            if let Some(event) = state.emit_tool_call(tool_call)? {
                                emitted.push(event);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(event_type @ ("response.completed" | "response.done")) => {
            state.saw_completion = true;
            let is_completed = event_type == "response.completed";
            if let Some(response) = event
                .get("response")
                .and_then(|value| serde_json::from_value::<ResponsesResponse>(value.clone()).ok())
            {
                if !state.saw_text_delta && state.fallback_text.is_none() {
                    state.fallback_text = extract_responses_text(&response);
                }
                for tool_call in extract_responses_tool_calls(&response) {
                    if let Some(event) = state.emit_tool_call(tool_call)? {
                        emitted.push(event);
                    }
                }
                if is_completed && let Some(usage) = parse_responses_usage(response.usage.as_ref())
                {
                    emitted.push(StreamEvent::Usage(usage));
                }
            }
        }
        _ => {}
    }

    Ok(emitted)
}

pub(crate) fn process_sse_chunk(
    chunk: &str,
    state: &mut ResponsesStreamState,
) -> anyhow::Result<Vec<StreamEvent>> {
    let data_lines: Vec<String> = chunk
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| line.trim().to_string())
        .collect();
    if data_lines.is_empty() {
        return Ok(Vec::new());
    }

    let joined = data_lines.join("\n");
    let trimmed = joined.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        if trimmed == "[DONE]" {
            state.saw_completion = true;
        }
        return Ok(Vec::new());
    }

    if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
        return process_responses_stream_event(event, state);
    }

    let mut emitted = Vec::new();
    for line in data_lines {
        let line = line.trim();
        if line.is_empty() || line == "[DONE]" {
            if line == "[DONE]" {
                state.saw_completion = true;
            }
            continue;
        }
        let event = serde_json::from_str::<Value>(line).map_err(|err| {
            let sanitized = super::sanitize_api_error(line);
            ::reiclaw_log::record!(
                ERROR,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "phase": "sse_parse",
                        "payload": &sanitized,
                        "error": format!("{}", err),
                    })),
                "OpenAI Responses: SSE data parse failed"
            );
            anyhow::Error::msg(format!(
                "OpenAI Responses SSE data parse failed: {err}. Payload: {sanitized}"
            ))
        })?;
        emitted.extend(process_responses_stream_event(event, state)?);
    }

    Ok(emitted)
}

pub(crate) fn extract_stream_error_message(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);

    if event_type == Some("error") {
        return first_nonempty(
            event
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| event.get("code").and_then(Value::as_str))
                .or_else(|| {
                    event
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                }),
        );
    }

    if event_type == Some("response.failed") {
        return first_nonempty(
            event
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
        );
    }

    None
}

#[derive(Default)]
pub(crate) struct ResponsesSseBuffer {
    pending: Vec<u8>,
    line_start: usize,
    validated: usize,
}

impl ResponsesSseBuffer {
    /// Consume at most one event. Apply the limit before copying each fragment,
    /// so a large HTTP chunk containing many small events is still accepted.
    pub(crate) fn next_frame(&mut self, bytes: &mut &[u8]) -> StreamResult<Option<String>> {
        while !bytes.is_empty() {
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let take = newline.unwrap_or(bytes.len());
            if take > MAX_SSE_FRAME_BYTES.saturating_sub(self.pending.len()) {
                return Err(Self::oversized_frame());
            }
            self.pending.extend_from_slice(&bytes[..take]);
            *bytes = &bytes[take + usize::from(newline.is_some())..];
            match std::str::from_utf8(&self.pending[self.validated..]) {
                Ok(_) => self.validated = self.pending.len(),
                Err(err) if newline.is_none() && err.error_len().is_none() => {
                    self.validated += err.valid_up_to();
                }
                Err(_) => return Err(Self::invalid_utf8()),
            }
            if newline.is_none() {
                return Ok(None);
            }
            if matches!(&self.pending[self.line_start..], b"" | b"\r") {
                self.pending.truncate(self.line_start);
                return self.take_frame().map(Some);
            }
            self.pending.push(b'\n');
            self.line_start = self.pending.len();
            self.validated = self.line_start;
        }
        Ok(None)
    }

    pub(crate) fn take_frame(&mut self) -> StreamResult<String> {
        self.line_start = 0;
        self.validated = 0;
        String::from_utf8(std::mem::take(&mut self.pending)).map_err(|_| Self::invalid_utf8())
    }

    fn oversized_frame() -> StreamError {
        StreamError::ModelProvider(format!("SSE frame exceeds {MAX_SSE_FRAME_BYTES} bytes"))
    }

    fn invalid_utf8() -> StreamError {
        StreamError::ModelProvider("SSE response contains invalid or incomplete UTF-8".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_reject_oversized_fragments_without_rejecting_large_http_chunks() {
        let mut buffer = ResponsesSseBuffer::default();
        let large = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        let mut oversized = large.as_slice();
        assert!(buffer.next_frame(&mut oversized).is_err());
        assert!(buffer.pending.is_empty());

        assert!(
            buffer
                .next_frame(&mut &large[..MAX_SSE_FRAME_BYTES])
                .unwrap()
                .is_none()
        );
        assert!(buffer.next_frame(&mut &b"x"[..]).is_err());
        let frame = buffer.next_frame(&mut &b"\n\n"[..]).unwrap().unwrap();
        assert_eq!(frame.trim_end().len(), MAX_SSE_FRAME_BYTES);

        let chunk = b"data: [DONE]\r\n\r\n".repeat(MAX_SSE_FRAME_BYTES / 15);
        assert!(chunk.len() > MAX_SSE_FRAME_BYTES);
        let mut rest = chunk.as_slice();
        while !rest.is_empty() {
            assert_eq!(
                buffer.next_frame(&mut rest).unwrap().unwrap(),
                "data: [DONE]\r\n"
            );
        }
    }

    #[test]
    fn sse_frames_preserve_fragmented_utf8_and_reject_invalid_or_truncated_utf8() {
        let mut buffer = ResponsesSseBuffer::default();
        for byte in "data: 測試\r\n".as_bytes() {
            assert!(
                buffer
                    .next_frame(&mut std::slice::from_ref(byte))
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            buffer.next_frame(&mut &b"\n"[..]).unwrap().unwrap(),
            "data: 測試\r\n"
        );
        assert!(buffer.next_frame(&mut &b"\xFF"[..]).is_err());

        let mut truncated = ResponsesSseBuffer::default();
        assert!(
            truncated
                .next_frame(&mut &b"\xe2\x82"[..])
                .unwrap()
                .is_none()
        );
        assert!(truncated.take_frame().is_err());
    }

    #[test]
    fn sse_tool_deltas_have_response_wide_count_and_byte_limits() {
        let mut state = ResponsesStreamState::default();
        for i in 0..=MAX_STREAM_TOOL_CALLS {
            let result = process_responses_stream_event(
                serde_json::json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": format!("tool_{i}"),
                    "delta": "{"
                }),
                &mut state,
            );
            assert_eq!(result.is_ok(), i < MAX_STREAM_TOOL_CALLS);
        }
        assert_eq!(state.tool_calls.len(), MAX_STREAM_TOOL_CALLS);

        let mut state = ResponsesStreamState::default();
        let delta = "x".repeat(MAX_STREAM_TOOL_BYTES / 8 - 1);
        for i in 0..9 {
            let result = process_responses_stream_event(
                serde_json::json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "a",
                    "delta": delta
                }),
                &mut state,
            );
            assert_eq!(result.is_ok(), i < 8);
        }
        assert_eq!(state.tool_calls["a"].arguments.len(), 8 * delta.len());
    }

    #[test]
    fn parses_responses_usage_without_synthesizing_missing_fields() {
        let usage = parse_responses_usage(Some(&serde_json::json!({
            "input_tokens": 120,
            "input_tokens_details": {"cached_tokens": 45},
            "output_tokens": 30
        })))
        .expect("valid usage should be reported");
        assert_eq!(usage.input_tokens, Some(120));
        assert_eq!(usage.cached_input_tokens, Some(45));
        assert_eq!(usage.output_tokens, Some(30));

        let partial = parse_responses_usage(Some(&serde_json::json!({
            "input_tokens": "not-a-number",
            "output_tokens": 7,
            "input_tokens_details": {"cached_tokens": null}
        })))
        .expect("a valid optional field should survive malformed siblings");
        assert_eq!(partial.input_tokens, None);
        assert_eq!(partial.cached_input_tokens, None);
        assert_eq!(partial.output_tokens, Some(7));
        assert!(parse_responses_usage(None).is_none());
        assert!(parse_responses_usage(Some(&serde_json::json!({}))).is_none());
    }

    #[test]
    fn completed_stream_event_emits_authoritative_usage() {
        let mut state = ResponsesStreamState::default();
        let events = process_sse_chunk(
            "data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"done\",\"usage\":{\"input_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":45},\"output_tokens\":30}}}",
            &mut state,
        )
        .expect("completed event should parse");

        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Usage(TokenUsage {
                input_tokens: Some(120),
                cached_input_tokens: Some(45),
                output_tokens: Some(30),
            })]
        ));
    }

    #[test]
    fn completed_stream_event_omits_usage_when_provider_does_not_report_it() {
        let mut state = ResponsesStreamState::default();
        let events = process_sse_chunk(
            "data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"done\"}}",
            &mut state,
        )
        .expect("completed event should parse");

        assert!(events.is_empty());
    }

    #[test]
    fn completed_stream_event_orders_fallback_tool_call_before_usage() {
        let mut state = ResponsesStreamState::default();
        let events = process_sse_chunk(
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"echo\",\"arguments\":\"{}\"}],\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}",
            &mut state,
        )
        .expect("completed event should parse");

        assert!(matches!(
            events.as_slice(),
            [StreamEvent::ToolCall(_), StreamEvent::Usage(_)]
        ));
    }

    #[test]
    fn done_alias_does_not_emit_usage() {
        let mut state = ResponsesStreamState::default();
        let events = process_sse_chunk(
            "data: {\"type\":\"response.done\",\"response\":{\"output_text\":\"done\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}",
            &mut state,
        )
        .expect("done alias should parse");

        assert!(events.is_empty());
    }

    #[test]
    fn process_sse_chunk_marks_completion_on_response_completed() {
        let mut state = ResponsesStreamState::default();
        let _ = process_sse_chunk(
            "data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"hi\"}}",
            &mut state,
        )
        .unwrap();
        assert!(state.saw_completion);
    }

    #[test]
    fn process_sse_chunk_marks_completion_on_done_sentinel() {
        let mut state = ResponsesStreamState::default();
        let _ = process_sse_chunk("data: [DONE]", &mut state).unwrap();
        assert!(state.saw_completion);
    }

    #[test]
    fn process_sse_chunk_leaves_completion_unset_on_text_delta() {
        let mut state = ResponsesStreamState::default();
        let _ = process_sse_chunk(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}",
            &mut state,
        )
        .unwrap();
        assert!(!state.saw_completion);
    }

    #[test]
    fn build_responses_input_maps_content_types_by_role() {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "You are helpful.".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Hi".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "Hello!".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Thanks".into(),
            },
        ];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, "You are helpful.");
        assert_eq!(input.len(), 3);

        let json: Vec<Value> = input
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        assert_eq!(json[0]["role"], "user");
        assert_eq!(json[0]["content"][0]["type"], "input_text");
        assert_eq!(json[1]["role"], "assistant");
        assert_eq!(json[1]["content"][0]["type"], "output_text");
        assert_eq!(json[2]["role"], "user");
        assert_eq!(json[2]["content"][0]["type"], "input_text");
    }

    #[test]
    fn build_responses_input_uses_default_instructions_without_system() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "Hello".into(),
        }];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, DEFAULT_INSTRUCTIONS);
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn build_responses_input_maps_tool_outputs() {
        let messages = vec![
            ChatMessage {
                role: "tool".into(),
                content: r#"{"tool_call_id":"call_123","content":"result"}"#.into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Go".into(),
            },
        ];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, DEFAULT_INSTRUCTIONS);
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_123");
        assert_eq!(input[0]["output"], "result");
        assert_eq!(input[1]["role"], "user");
    }

    #[test]
    fn build_responses_input_replays_plain_tool_text_without_synthetic_call_id() {
        let messages = vec![ChatMessage {
            role: "tool".into(),
            content: "legacy plain text result".into(),
        }];

        let (_, input) = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(
            input[0]["content"][0]["text"],
            "Legacy tool output without call_id:\nlegacy plain text result"
        );
        assert!(input[0].get("call_id").is_none());
    }

    #[test]
    fn build_responses_input_replays_tool_json_without_call_id_as_text() {
        let messages = vec![ChatMessage {
            role: "tool".into(),
            content: r#"{"content":"legacy result","status":"ok"}"#.into(),
        }];

        let (_, input) = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(
            input[0]["content"][0]["text"],
            r#"Legacy tool output without call_id:
{"content":"legacy result","status":"ok"}"#
        );
        assert!(input[0].get("call_id").is_none());
    }

    #[test]
    fn build_responses_input_replays_blank_tool_call_id_as_legacy_text() {
        for raw_id in ["", "   "] {
            let messages = vec![ChatMessage {
                role: "tool".into(),
                content: serde_json::json!({
                    "tool_call_id": raw_id,
                    "content": "legacy result"
                })
                .to_string(),
            }];

            let (_, input) = build_responses_input(&messages);

            assert_eq!(input.len(), 1);
            assert_eq!(input[0]["type"], "message");
            assert_eq!(input[0]["role"], "user");
            assert_eq!(input[0]["content"][0]["type"], "input_text");
            assert!(input[0].get("call_id").is_none());
            assert!(
                input[0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("\"legacy result\"")
            );
        }
    }

    #[test]
    fn build_responses_input_maps_native_assistant_tool_calls() {
        let messages = vec![ChatMessage::assistant(
            r#"{"content":"Using shell","tool_calls":[{"id":"call_abc","name":"shell","arguments":"{\"command\":\"pwd\"}"}]}"#,
        )];
        let (_, input) = build_responses_input(&messages);

        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_abc");
        assert_eq!(input[1]["name"], "shell");
    }

    #[test]
    fn convert_tools_opts_out_of_responses_strict_mode() {
        let tools = vec![ToolSpec::new(
            "jira",
            "Interact with Jira",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string" },
                    "issue_key": { "type": "string" }
                },
                "required": ["action"]
            }),
        )];

        let converted = convert_tools(Some(&tools)).expect("tool should convert");
        let value = serde_json::to_value(&converted[0]).expect("tool should serialize");
        assert_eq!(value["type"], "function");
        assert_eq!(value["name"], "jira");
        assert_eq!(value["strict"], false);
        assert_eq!(value["parameters"]["required"][0], "action");
    }

    #[test]
    fn build_responses_input_handles_image_markers() {
        let messages = vec![ChatMessage::user(
            "Describe this\n\n[IMAGE:data:image/png;base64,abc]",
        )];
        let (_, input) = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 2);

        let json = input[0]["content"].as_array().unwrap();

        // First content = text
        assert_eq!(json[0]["type"], "input_text");
        assert!(json[0]["text"].as_str().unwrap().contains("Describe this"));

        // Second content = image
        assert_eq!(json[1]["type"], "input_image");
        assert_eq!(json[1]["image_url"], "data:image/png;base64,abc");
    }

    #[test]
    fn build_responses_input_preserves_text_only_messages() {
        let messages = vec![ChatMessage::user("Hello without images")];
        let (_, input) = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 1);

        let json = &input[0]["content"][0];
        assert_eq!(json["type"], "input_text");
        assert_eq!(json["text"], "Hello without images");
    }

    #[test]
    fn build_responses_input_handles_multiple_images() {
        let messages = vec![ChatMessage::user(
            "Compare these: [IMAGE:data:image/png;base64,img1] and [IMAGE:data:image/jpeg;base64,img2]",
        )];
        let (_, input) = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 3); // text + 2 images

        let json = input[0]["content"].as_array().unwrap();

        assert_eq!(json[0]["type"], "input_text");
        assert_eq!(json[1]["type"], "input_image");
        assert_eq!(json[2]["type"], "input_image");
    }

    #[test]
    fn build_function_call_item_sanitizes_invalid_arguments_to_empty_object() {
        // Pins that the openai_codex call site of `sanitize_tool_arguments`
        // is wired in. The helper contract itself is covered in
        // `compatible::tests::sanitize_tool_arguments_*`.
        let call = ProviderToolCall {
            id: "call_bad".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"rm -rf"#.to_string(),
            extra_content: None,
        };

        let item = build_function_call_item(call);
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "call_bad");
        assert_eq!(item["name"], "shell");
        assert_eq!(item["arguments"], "{}");
    }

    #[test]
    fn build_function_call_item_passes_through_valid_arguments() {
        // Companion regression: valid JSON must round-trip byte-for-byte.
        let call = ProviderToolCall {
            id: "call_ok".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"pwd"}"#.to_string(),
            extra_content: None,
        };

        let item = build_function_call_item(call);
        assert_eq!(item["arguments"], r#"{"command":"pwd"}"#);
    }
}
