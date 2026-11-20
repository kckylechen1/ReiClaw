//! Host scope for point reads/deletes over Tachi memory entries;
//! physical keys/paths are backend details, never session authority.

use crate::traits::{MemoryCategory, MemoryEntry};

/// No task-local context is the historical administrative API. An installed
/// context, even an empty one, cannot see another session's Conversation rows.
/// Secondary compatibility aliases are not scope.
pub(crate) struct PointScope(Option<Option<String>>);

impl PointScope {
    pub(crate) fn current() -> Self {
        Self(
            reiclaw_api::TOOL_LOOP_MEMORY_SESSIONS
                .try_with(|sessions| {
                    sessions
                        .first()
                        .filter(|sid| !sid.trim().is_empty())
                        .cloned()
                })
                .ok(),
        )
    }

    pub(crate) fn select(
        &self,
        mut rows: Vec<MemoryEntry>,
        include_soul: bool,
    ) -> Vec<MemoryEntry> {
        rows.retain(|row| include_soul || row.namespace != crate::SOUL_NAMESPACE);
        if let Some(session) = &self.0 {
            rows.retain(|row| {
                row.category != MemoryCategory::Conversation
                    || session
                        .as_deref()
                        .is_some_and(|sid| row.session_id.as_deref() == Some(sid))
            });
            // A point key resolves to its Conversation row in this session,
            // falling back to the global key separately for each agent.
            let scoped_agents: std::collections::HashSet<String> = rows
                .iter()
                .filter(|row| row.category == MemoryCategory::Conversation)
                .map(|row| row.agent_id.as_deref().unwrap_or("default").to_owned())
                .collect();
            rows.retain(|row| {
                row.category == MemoryCategory::Conversation
                    || !scoped_agents.contains(row.agent_id.as_deref().unwrap_or("default"))
            });
        }
        rows
    }
}
