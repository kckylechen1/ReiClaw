//! Results collection: preserve call order, media markers, receipts and
//! display truncation. Repetition evidence is owned by the turn guard.

use crate::agent::history::{canonicalize_tool_result_media_markers_for, truncate_tool_result};
use crate::agent::tool_execution::ToolExecutionOutcome;
use std::fmt::Write;
use std::sync::Mutex;

/// One round's collected tool results.
pub(crate) struct CollectedResults {
    /// Per-call `(tool_call_id, output)` so native-mode history can emit one
    /// `role=tool` message per call with the correct ID.
    pub(crate) individual_results: Vec<(Option<String>, String)>,
    /// XML `<tool_result>` blocks for prompt-mode history.
    pub(crate) tool_results: String,
}

/// Collect every completed or synthetic result without choosing whether the
/// turn continues. The caller appends these results before any disposition.
pub(crate) fn collect_tool_results(
    ordered_results: Vec<Option<(String, Option<String>, ToolExecutionOutcome)>>,
    max_tool_result_chars: usize,
    collected_receipts: Option<&Mutex<Vec<String>>>,
) -> CollectedResults {
    let mut tool_results = String::new();
    let mut individual_results: Vec<(Option<String>, String)> = Vec::new();
    for (tool_name, tool_call_id, outcome) in ordered_results.into_iter().flatten() {
        let canonical_output =
            canonicalize_tool_result_media_markers_for(&tool_name, &outcome.output);
        let mut result_output = truncate_tool_result(&canonical_output, max_tool_result_chars);
        // Append HMAC receipt to tool result when receipts are enabled
        if let Some(ref receipt) = outcome.receipt {
            ::reiclaw_log::record!(
                DEBUG,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_category(::reiclaw_log::EventCategory::Tool)
                    .with_attrs(::serde_json::json!({"tool": tool_name, "receipt": receipt})),
                "Tool receipt generated"
            );
            result_output = format!("{result_output}\n\n[receipt: {receipt}]");
            if let Some(store) = collected_receipts
                && let Ok(mut v) = store.lock()
            {
                v.push(format!("{tool_name}: {receipt}"));
            }
        }
        individual_results.push((tool_call_id, result_output.clone()));
        let _ = writeln!(
            tool_results,
            "<tool_result name=\"{}\">\n{}\n</tool_result>",
            tool_name, result_output
        );
    }

    CollectedResults {
        individual_results,
        tool_results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn collection_preserves_sparse_call_order_failures_truncation_and_receipts() {
        let receipts = Mutex::new(Vec::new());
        let successful = ToolExecutionOutcome {
            output: "begin ".to_string() + &"x".repeat(100) + " end",
            success: true,
            error_reason: None,
            duration: Duration::ZERO,
            receipt: Some("signed-test-receipt".into()),
        };
        let failed = ToolExecutionOutcome {
            output: "recoverable tool failure".into(),
            success: false,
            error_reason: Some("recoverable tool failure".into()),
            duration: Duration::ZERO,
            receipt: None,
        };
        let collected = collect_tool_results(
            vec![
                Some(("first".into(), Some("call-1".into()), successful)),
                None,
                Some(("last".into(), Some("call-3".into()), failed)),
            ],
            30,
            Some(&receipts),
        );
        assert_eq!(collected.individual_results.len(), 2);
        let (first_id, first_output) = &collected.individual_results[0];
        assert_eq!(first_id.as_deref(), Some("call-1"));
        assert!(first_output.starts_with("begin "));
        assert!(first_output.contains("characters truncated"));
        assert!(first_output.ends_with("end\n\n[receipt: signed-test-receipt]"));
        assert_eq!(
            collected.individual_results[1],
            (Some("call-3".into()), "recoverable tool failure".into()),
        );
        assert_eq!(
            receipts.lock().unwrap().as_slice(),
            ["first: signed-test-receipt"]
        );
        assert_eq!(
            collected.tool_results,
            format!(
                "<tool_result name=\"first\">\n{first_output}\n</tool_result>\n\
                 <tool_result name=\"last\">\nrecoverable tool failure\n</tool_result>\n"
            ),
        );
    }
}
