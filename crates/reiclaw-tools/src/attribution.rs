//! Centralized `Attributable` impls for every concrete `Tool` in this
//! crate. Each invocation surfaces `Role::Tool(ToolKind::*)` and uses
//! the tool's `name()` as its alias so log emissions can attribute
//! tool activity with the same `<kind>.<alias>` composite the rest of

use reiclaw_api::attribution::ToolKind;
use reiclaw_api::tool_attribution;

use crate::ask_user::AskUserTool;
use crate::backup_tool::BackupTool;
use crate::content_search::ContentSearchTool;
use crate::data_management::DataManagementTool;
use crate::escalate::EscalateToHumanTool;
use crate::file_edit::FileEditTool;
use crate::file_write::FileWriteTool;
use crate::glob_search::GlobSearchTool;
use crate::http_request::HttpRequestTool;
use crate::mcp_tool::McpToolWrapper;
use crate::memory_export::MemoryExportTool;
use crate::memory_forget::MemoryForgetTool;
use crate::memory_purge::MemoryPurgeTool;
use crate::memory_recall::MemoryRecallTool;
use crate::memory_store::MemoryStoreTool;
use crate::propose_soul_change::ProposeSoulChangeTool;
use crate::screenshot::ScreenshotTool;
use crate::sessions::{
    SessionsCurrentTool, SessionsHistoryTool, SessionsListTool, SessionsSendTool,
};
use crate::tool_search::ToolSearchTool;
use crate::web_fetch::WebFetchTool;
use crate::web_search_tool::WebSearchTool;

tool_attribution!(AskUserTool, ToolKind::Wait);
tool_attribution!(BackupTool, ToolKind::Plugin);
tool_attribution!(ContentSearchTool, ToolKind::Search);
tool_attribution!(DataManagementTool, ToolKind::Plugin);
tool_attribution!(EscalateToHumanTool, ToolKind::Wait);
tool_attribution!(FileEditTool, ToolKind::Plugin);
tool_attribution!(FileWriteTool, ToolKind::Plugin);
tool_attribution!(GlobSearchTool, ToolKind::Search);
tool_attribution!(HttpRequestTool, ToolKind::HttpRequest);
tool_attribution!(McpToolWrapper, ToolKind::Plugin);
tool_attribution!(MemoryExportTool, ToolKind::Memory);
tool_attribution!(MemoryForgetTool, ToolKind::Memory);
tool_attribution!(ProposeSoulChangeTool, ToolKind::Memory);
tool_attribution!(
    crate::note_owner_correction::NoteOwnerCorrectionTool,
    ToolKind::Memory
);
tool_attribution!(MemoryPurgeTool, ToolKind::Memory);
tool_attribution!(MemoryRecallTool, ToolKind::Memory);
tool_attribution!(MemoryStoreTool, ToolKind::Memory);
tool_attribution!(ScreenshotTool, ToolKind::Plugin);
tool_attribution!(SessionsCurrentTool, ToolKind::Plugin);
tool_attribution!(SessionsHistoryTool, ToolKind::Plugin);
tool_attribution!(SessionsListTool, ToolKind::Plugin);
tool_attribution!(SessionsSendTool, ToolKind::Plugin);
tool_attribution!(ToolSearchTool, ToolKind::Search);
tool_attribution!(WebFetchTool, ToolKind::FetchUrl);
tool_attribution!(WebSearchTool, ToolKind::Search);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use reiclaw_api::attribution::{Attributable, Role};
    use reiclaw_policy::SecurityPolicy;

    fn subject() -> FileWriteTool {
        FileWriteTool::new(Arc::new(SecurityPolicy::default()))
    }

    #[test]
    fn macro_sets_role_to_tool_kind() {
        let tool = subject();
        assert_eq!(tool.role(), Role::Tool(ToolKind::Plugin));
        assert_eq!(tool.alias(), "file_write");
    }

    #[test]
    fn attributable_via_arc_matches_inner() {
        let inner = subject();
        let arc: Arc<FileWriteTool> = Arc::new(inner);
        assert_eq!(arc.alias(), "file_write");
        assert_eq!(arc.role(), Role::Tool(ToolKind::Plugin));
    }

    #[test]
    fn tool_name_has_no_dot_separator() {
        let tool = subject();
        assert!(
            !tool.alias().contains('.'),
            "alias `{}` must not contain `.` — would break `<kind>.<alias>` composite parsing",
            tool.alias()
        );
    }

    #[test]
    fn tool_name_is_nonempty() {
        let tool = subject();
        assert!(!tool.alias().is_empty());
    }
}
