use super::history::canonicalize_tool_result_media_markers_for;
use crate::tools::Tool;
use reiclaw_providers::{ChatMessage, ConversationMessage};
use std::fmt::Write;

pub trait ToolDispatcher: Send + Sync {
    fn prompt_instructions(&self, tools: &[Box<dyn Tool>]) -> String;
    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage>;
    fn should_send_tool_specs(&self) -> bool;
}

#[derive(Default)]
pub struct XmlToolDispatcher;

impl ToolDispatcher for XmlToolDispatcher {
    fn prompt_instructions(&self, tools: &[Box<dyn Tool>]) -> String {
        if tools.is_empty() {
            return String::new();
        }

        let mut instructions = String::new();
        instructions.push_str("## Tool Use Protocol\n\n");
        instructions
            .push_str("To use a tool, wrap a JSON object in <tool_call></tool_call> tags:\n\n");
        instructions.push_str(
            "```\n<tool_call>\n{\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n</tool_call>\n```\n\n",
        );

        instructions
    }

    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage> {
        history
            .iter()
            .flat_map(|msg| match msg {
                ConversationMessage::Chat(chat) => vec![chat.clone()],
                ConversationMessage::AssistantToolCalls { text, .. } => {
                    vec![ChatMessage::assistant(text.clone().unwrap_or_default())]
                }
                ConversationMessage::ToolResults(results) => {
                    let mut content = String::new();
                    for result in results {
                        let output = canonicalize_tool_result_media_markers_for(
                            &result.tool_name,
                            &result.content,
                        );
                        let _ = writeln!(
                            content,
                            "<tool_result id=\"{}\">\n{}\n</tool_result>",
                            result.tool_call_id, output
                        );
                    }
                    vec![ChatMessage::user(format!("[Tool results]\n{content}"))]
                }
            })
            .collect()
    }

    fn should_send_tool_specs(&self) -> bool {
        false
    }
}

pub struct NativeToolDispatcher;

impl ToolDispatcher for NativeToolDispatcher {
    fn prompt_instructions(&self, _tools: &[Box<dyn Tool>]) -> String {
        String::new()
    }

    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage> {
        history
            .iter()
            .flat_map(|msg| match msg {
                ConversationMessage::Chat(chat) => vec![chat.clone()],
                ConversationMessage::AssistantToolCalls {
                    text,
                    tool_calls,
                    reasoning_content,
                } => {
                    let mut payload = serde_json::json!({
                        "content": text,
                        "tool_calls": tool_calls,
                    });
                    if let Some(rc) = reasoning_content {
                        payload["reasoning_content"] = serde_json::json!(rc);
                    }
                    vec![ChatMessage::assistant(payload.to_string())]
                }
                ConversationMessage::ToolResults(results) => results
                    .iter()
                    .map(|result| {
                        ChatMessage::tool(
                            serde_json::json!({
                                "tool_call_id": result.tool_call_id,
                                "content": canonicalize_tool_result_media_markers_for(
                                    &result.tool_name,
                                    &result.content,
                                ),
                            })
                            .to_string(),
                        )
                    })
                    .collect(),
            })
            .collect()
    }

    fn should_send_tool_specs(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reiclaw_providers::ToolResultMessage;

    /// Write a throwaway PNG and return its absolute path string. An existing
    /// local image path is required for canonicalization to fire at all.
    fn write_temp_image(dir: &std::path::Path, name: &str) -> String {
        let image = dir.join(name);
        std::fs::write(&image, [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']).unwrap();
        image.display().to_string()
    }

    fn native_tool_message_content(message: &ChatMessage) -> String {
        let payload: serde_json::Value = serde_json::from_str(&message.content).unwrap();
        payload["content"].as_str().unwrap().to_owned()
    }

    fn native_to_provider_tool_content(
        dispatcher: &NativeToolDispatcher,
        tool_name: &str,
        output: String,
    ) -> String {
        let history = vec![ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: output,
            tool_name: tool_name.into(),
        }])];
        let messages = dispatcher.to_provider_messages(&history);
        assert_eq!(messages.len(), 1, "one tool result -> one provider message");
        assert_eq!(messages[0].role, "tool");
        native_tool_message_content(&messages[0])
    }

    #[test]
    fn native_search_path_survives_to_provider_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_image(dir.path(), "hit.png");
        let native = NativeToolDispatcher;

        for tool in ["content_search", "glob_search"] {
            let rendered = native_to_provider_tool_content(&native, tool, format!("found: {path}"));
            assert!(
                !rendered.contains("[IMAGE:"),
                "{tool} path must not be re-promoted on the read side"
            );
            assert!(
                rendered.contains(&path),
                "{tool} provider-visible content must keep the literal path"
            );
        }
    }

    #[test]
    fn native_screenshot_path_still_promotes_through_to_provider_messages() {
        // Default-allow preserved across the round trip: a real generated image
        // still becomes an `[IMAGE:...]` marker, so it routes to a vision
        // provider as before.
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_image(dir.path(), "generated.png");
        let native = NativeToolDispatcher;

        let rendered =
            native_to_provider_tool_content(&native, "screenshot", format!("saved to {path}"));
        assert!(
            rendered.contains(&format!("[IMAGE:{path}]")),
            "screenshot image must still canonicalize through the round trip"
        );
    }

    #[test]
    fn native_unknown_provenance_still_promotes_on_read() {
        // contract preserved: a tool result stored WITHOUT provenance
        // (empty `tool_name`, e.g. reconstructed from a provider-wire message)
        // still has a genuine image path canonicalized on the read side.
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_image(dir.path(), "history.png");
        let native = NativeToolDispatcher;

        let history = vec![ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: format!("Saved image to {path}"),
            tool_name: String::new(),
        }])];
        let messages = native.to_provider_messages(&history);
        assert_eq!(messages.len(), 1);
        assert!(
            native_tool_message_content(&messages[0]).contains(&format!("[IMAGE:{path}]")),
            "unknown-provenance result must still promote a real image path"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // reasoning_content pass-through tests
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn native_to_provider_messages_includes_reasoning_content() {
        let dispatcher = NativeToolDispatcher;
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("answer".into()),
            tool_calls: vec![reiclaw_providers::ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
                extra_content: None,
            }],
            reasoning_content: Some("thinking step".into()),
        }];

        let messages = dispatcher.to_provider_messages(&history);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "assistant");

        let payload: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        assert_eq!(payload["reasoning_content"].as_str(), Some("thinking step"));
        assert_eq!(payload["content"].as_str(), Some("answer"));
        assert!(payload["tool_calls"].is_array());
    }

    #[test]
    fn native_to_provider_messages_omits_reasoning_content_when_none() {
        let dispatcher = NativeToolDispatcher;
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("answer".into()),
            tool_calls: vec![reiclaw_providers::ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
                extra_content: None,
            }],
            reasoning_content: None,
        }];

        let messages = dispatcher.to_provider_messages(&history);
        assert_eq!(messages.len(), 1);

        let payload: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        assert!(payload.get("reasoning_content").is_none());
    }

    #[test]
    fn xml_to_provider_messages_ignores_reasoning_content() {
        let dispatcher = XmlToolDispatcher;
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("answer".into()),
            tool_calls: vec![reiclaw_providers::ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
                extra_content: None,
            }],
            reasoning_content: Some("should be ignored".into()),
        }];

        let messages = dispatcher.to_provider_messages(&history);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "assistant");
        // XmlToolDispatcher returns text only, not JSON payload
        assert_eq!(messages[0].content, "answer");
        assert!(!messages[0].content.contains("reasoning_content"));
    }
}
