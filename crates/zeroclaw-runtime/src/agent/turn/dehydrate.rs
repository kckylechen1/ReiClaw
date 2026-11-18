//! Cold-window dehydration for stale tool outputs and oversized delegated
//! reports outside the active `keep_tool_context_turns` window (ADR-025).

use std::borrow::Cow;
use zeroclaw_providers::ChatMessage;
use zeroclaw_providers::multimodal::{
    STALE_TOOL_RESULT_MAX_CHARS, TOOL_PARSE_ERROR_FEEDBACK_PREFIX,
};

fn is_prompt_tool_result_message(message: &ChatMessage) -> bool {
    message.role == "user" && message.content.trim_start().starts_with("[Tool results]")
}

/// Byte threshold above which stale tool outputs outside the hot turn window
/// are dehydrated into a compact ~300-word preview digest.
pub(crate) const STALE_TOOL_OUTPUT_DEHYDRATE_BYTES: usize = 2_400;
/// Byte threshold above which stale delegated report (`result.md`) blocks
/// outside the hot turn window are dehydrated into a compact ~300-word digest.
pub(crate) const DELEGATED_REPORT_DEHYDRATE_BYTES: usize = STALE_TOOL_RESULT_MAX_CHARS;

const DEHYDRATED_HEAD_WORDS: usize = 200;
const DEHYDRATED_TAIL_WORDS: usize = 80;
const DEHYDRATED_HEAD_MAX_BYTES: usize = 1_200;
const DEHYDRATED_TAIL_MAX_BYTES: usize = 480;

fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut pos = index;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut pos = index;
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos
}

fn is_real_user_turn(message: &ChatMessage) -> bool {
    if message.role != "user" {
        return false;
    }
    let trimmed = message.content.trim_start();
    !is_prompt_tool_result_message(message)
        && !trimmed.starts_with(TOOL_PARSE_ERROR_FEEDBACK_PREFIX)
        && !crate::agent::history_trim::is_session_checkpoint_message(message)
        && message.content != crate::agent::history_trim::breadcrumb().content
}

fn cold_window_cutoff(messages: &[ChatMessage], keep_tool_context_turns: usize) -> usize {
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| is_real_user_turn(message))
        .nth(keep_tool_context_turns)
        .map_or(0, |(index, _)| index)
}

fn is_delegated_report_block(text: &str) -> bool {
    text.len() > DELEGATED_REPORT_DEHYDRATE_BYTES
        && (text.contains("result.md")
            || text.contains("untrusted_external_report")
            || text.contains("tachi_result"))
}

fn is_assistant_reasoning_message(message: &ChatMessage) -> bool {
    if message.role != "assistant" {
        return false;
    }
    if let Ok(serde_json::Value::Object(obj)) =
        serde_json::from_str::<serde_json::Value>(&message.content)
    {
        return obj.contains_key("reasoning_content") || obj.contains_key("tool_calls");
    }
    let trimmed = message.content.trim_start();
    trimmed.starts_with("Thoughts:")
        || trimmed.starts_with("<think>")
        || trimmed.contains("\nThoughts:")
        || trimmed.contains("\nwhy:")
}

fn head_end_index(text: &str) -> usize {
    let mut words = 0usize;
    let mut in_word = false;
    let mut end = text.len().min(DEHYDRATED_HEAD_MAX_BYTES);
    for (idx, ch) in text.char_indices() {
        if idx >= DEHYDRATED_HEAD_MAX_BYTES {
            end = idx;
            break;
        }
        if ch.is_whitespace() {
            if in_word {
                words += 1;
                in_word = false;
                if words >= DEHYDRATED_HEAD_WORDS {
                    end = idx;
                    break;
                }
            }
        } else {
            in_word = true;
        }
    }
    floor_char_boundary(text, end)
}

fn tail_start_index(text: &str) -> usize {
    let min_start = text.len().saturating_sub(DEHYDRATED_TAIL_MAX_BYTES);
    let mut words = 0usize;
    let mut in_word = false;
    let mut start = min_start;
    for (idx, ch) in text.char_indices().rev() {
        if idx < min_start {
            start = idx + ch.len_utf8();
            break;
        }
        if ch.is_whitespace() {
            if in_word {
                words += 1;
                in_word = false;
                if words >= DEHYDRATED_TAIL_WORDS {
                    start = idx + ch.len_utf8();
                    break;
                }
            }
        } else {
            in_word = true;
        }
    }
    ceil_char_boundary(text, start)
}

/// Dehydrate a single oversized text payload into a ~300-word head+tail preview
/// pointing to `session_search` and `episode_read` for full drill-down.
pub(crate) fn dehydrate_text_preview(text: &str, threshold_bytes: usize) -> String {
    if threshold_bytes == 0 || text.len() <= threshold_bytes {
        return text.to_string();
    }
    let head_end = head_end_index(text);
    let tail_start = tail_start_index(text);
    if head_end == 0 || head_end >= tail_start {
        return text.to_string();
    }

    let omitted_bytes = tail_start.saturating_sub(head_end);
    let omitted_words = text[head_end..tail_start].split_whitespace().count();
    let label = if is_delegated_report_block(text) {
        "delegated report (result.md)"
    } else {
        "stale tool output"
    };
    let candidate = format!(
        "{}\n\n[... dehydrated {label}: ~{omitted_words} words / {omitted_bytes} bytes omitted; drill down with session_search or episode_read ...]\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    );
    if candidate.len() >= text.len() || candidate.len() > STALE_TOOL_RESULT_MAX_CHARS {
        text.to_string()
    } else {
        candidate
    }
}

fn dehydrate_prompt_tool_results(content: &str) -> Option<String> {
    const PREFIX: &str = "[Tool results]";
    const CLOSE: &str = "\n</tool_result>";
    let trimmed = content.trim_start();
    let after_prefix = trimmed.strip_prefix(PREFIX)?;
    let prefix_end = content.len() - after_prefix.len();
    let mut out = content[..prefix_end].to_string();
    let mut remaining = after_prefix;
    let mut changed = false;

    loop {
        let trimmed = remaining.trim_start();
        out.push_str(&remaining[..remaining.len() - trimmed.len()]);
        remaining = trimmed;
        if remaining.is_empty() {
            return changed.then_some(out);
        }
        let after_open = remaining.strip_prefix("<tool_result ")?;
        let (attributes, body_and_rest) = after_open.split_once(">\n")?;
        if attributes.contains(['<', '>', '\n', '\r']) {
            return None;
        }
        let (body, rest) = body_and_rest.split_once(CLOSE)?;
        if body.contains("<tool_result") || body.contains("</tool_result") {
            return None;
        }
        let header_len = remaining.len() - body_and_rest.len();
        out.push_str(&remaining[..header_len]);
        let threshold = if is_delegated_report_block(body) {
            DELEGATED_REPORT_DEHYDRATE_BYTES
        } else {
            STALE_TOOL_OUTPUT_DEHYDRATE_BYTES
        };
        let compacted = dehydrate_text_preview(body, threshold);
        if compacted != body {
            changed = true;
        }
        out.push_str(&compacted);
        out.push_str(CLOSE);
        remaining = rest;
    }
}

fn dehydrate_cold_message(message: &ChatMessage) -> Option<ChatMessage> {
    if message.role == "system" || is_real_user_turn(message) {
        return None;
    }
    if crate::agent::history_trim::is_session_checkpoint_message(message)
        || message.content == crate::agent::history_trim::breadcrumb().content
    {
        return None;
    }

    if is_prompt_tool_result_message(message) {
        if let Some(compacted) = dehydrate_prompt_tool_results(&message.content) {
            return Some(ChatMessage {
                role: message.role.clone(),
                content: compacted,
            });
        }
        if message.content.len() > STALE_TOOL_OUTPUT_DEHYDRATE_BYTES {
            let compacted =
                dehydrate_text_preview(&message.content, STALE_TOOL_OUTPUT_DEHYDRATE_BYTES);
            if compacted != message.content {
                return Some(ChatMessage {
                    role: message.role.clone(),
                    content: compacted,
                });
            }
        }
        return None;
    }

    if message.role == "tool" {
        if let Ok(serde_json::Value::Object(mut obj)) =
            serde_json::from_str::<serde_json::Value>(&message.content)
            && let Some(serde_json::Value::String(inner)) = obj.get("content").cloned()
        {
            let threshold = if is_delegated_report_block(&inner) {
                DELEGATED_REPORT_DEHYDRATE_BYTES
            } else {
                STALE_TOOL_OUTPUT_DEHYDRATE_BYTES
            };
            let compacted = dehydrate_text_preview(&inner, threshold);
            if compacted != inner {
                obj.insert("content".to_string(), serde_json::Value::String(compacted));
                return Some(ChatMessage {
                    role: message.role.clone(),
                    content: serde_json::Value::Object(obj).to_string(),
                });
            }
            return None;
        }
        if message.content.len() > STALE_TOOL_OUTPUT_DEHYDRATE_BYTES {
            let compacted =
                dehydrate_text_preview(&message.content, STALE_TOOL_OUTPUT_DEHYDRATE_BYTES);
            if compacted != message.content {
                return Some(ChatMessage {
                    role: message.role.clone(),
                    content: compacted,
                });
            }
        }
        return None;
    }

    if message.role == "assistant"
        && !is_assistant_reasoning_message(message)
        && is_delegated_report_block(&message.content)
    {
        let compacted = dehydrate_text_preview(&message.content, DELEGATED_REPORT_DEHYDRATE_BYTES);
        if compacted != message.content {
            return Some(ChatMessage {
                role: message.role.clone(),
                content: compacted,
            });
        }
    }

    None
}

/// Project `history` with stale oversized tool outputs and `> 4_096`-byte
/// delegated report (`result.md`) blocks outside `keep_tool_context_turns`
/// dehydrated into compact ~300-word preview digests pointing to
/// `session_search` / `episode_read`. Never mutates canonical history.
pub(crate) fn dehydrate_cold_window(
    history: &[ChatMessage],
    keep_tool_context_turns: usize,
) -> Cow<'_, [ChatMessage]> {
    let cutoff = cold_window_cutoff(history, keep_tool_context_turns);
    if cutoff == 0 {
        return Cow::Borrowed(history);
    }

    let mut owned: Option<Vec<ChatMessage>> = None;
    for (idx, message) in history.iter().enumerate() {
        if idx >= cutoff {
            break;
        }
        if let Some(replacement) = dehydrate_cold_message(message) {
            let vec = owned.get_or_insert_with(|| history.to_vec());
            vec[idx] = replacement;
        }
    }

    match owned {
        Some(vec) => Cow::Owned(vec),
        None => Cow::Borrowed(history),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_window_dehydrates_stale_tool_and_delegated_report_while_preserving_user_and_reasoning()
    {
        let huge_tool_body = format!(
            "HEAD-LINE\n{}\nTAIL-LINE",
            "verbose compile output line ".repeat(220)
        );
        let huge_delegated_report = format!(
            "{{\"request_id\":\"req-42\",\"artifact\":\"result.md\",\"trust\":\"untrusted_external_report\",\"body\":\"{}\"}}",
            "detailed worker report finding ".repeat(260)
        );
        assert!(huge_delegated_report.len() > DELEGATED_REPORT_DEHYDRATE_BYTES);

        let user_msg = format!("User asked detailed question: {}", "detail ".repeat(500));
        let assistant_thoughts = format!(
            "Thoughts: why we chose this architecture:\n{}",
            "reasoning step ".repeat(300)
        );
        let hot_tool_body = format!("HOT-HEAD\n{}\nHOT-TAIL", "fresh output ".repeat(250));

        let history = vec![
            ChatMessage::system("[Pinned memory index]\nanchor\n[/Pinned memory index]"),
            ChatMessage::user(&user_msg),
            ChatMessage::assistant(&assistant_thoughts),
            ChatMessage::tool(
                serde_json::json!({"tool_call_id": "call-old-1", "content": huge_tool_body})
                    .to_string(),
            ),
            ChatMessage::tool(
                serde_json::json!({"tool_call_id": "call-old-rep", "content": huge_delegated_report})
                    .to_string(),
            ),
            ChatMessage::user("warm prior question"),
            ChatMessage::tool(
                serde_json::json!({"tool_call_id": "call-warm-1", "content": hot_tool_body})
                    .to_string(),
            ),
            ChatMessage::user("recent question"),
            ChatMessage::tool(
                serde_json::json!({"tool_call_id": "call-hot-1", "content": hot_tool_body})
                    .to_string(),
            ),
        ];

        let projected = dehydrate_cold_window(&history, 1);

        // System, user message, and assistant reasoning remain intact.
        assert_eq!(projected[0].content, history[0].content);
        assert_eq!(projected[1].content, user_msg);
        assert_eq!(projected[2].content, assistant_thoughts);

        // Cold tool output is dehydrated with head, tail, and drill-down handles.
        let cold_tool: serde_json::Value = serde_json::from_str(&projected[3].content).unwrap();
        let cold_inner = cold_tool["content"].as_str().unwrap();
        assert!(cold_inner.starts_with("HEAD-LINE"));
        assert!(cold_inner.ends_with("TAIL-LINE"));
        assert!(cold_inner.contains("session_search"));
        assert!(cold_inner.contains("episode_read"));
        assert!(cold_inner.len() <= STALE_TOOL_RESULT_MAX_CHARS);
        assert!(cold_inner.split_whitespace().count() <= 340);

        // Cold >4096-byte delegated report is dehydrated and cites result.md + drill-down tools.
        let cold_rep: serde_json::Value = serde_json::from_str(&projected[4].content).unwrap();
        let rep_inner = cold_rep["content"].as_str().unwrap();
        assert!(rep_inner.contains("delegated report (result.md)"));
        assert!(rep_inner.contains("session_search"));
        assert!(rep_inner.contains("episode_read"));
        assert!(rep_inner.len() < 2_500);

        // Warm prior turn and active turn tool outputs stay verbatim.
        assert_eq!(projected[6].content, history[6].content);
        assert_eq!(projected[8].content, history[8].content);
    }
}
