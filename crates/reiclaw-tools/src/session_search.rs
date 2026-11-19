//! Agent-scoped FTS5 session transcript search with chronology guardrails (ADR-025).

use async_trait::async_trait;
use reiclaw_api::sleep_records::validate_agent_id;
use reiclaw_api::tool::{Tool, ToolOutput, ToolResult};
use reiclaw_infra::session_backend::{SessionBackend, SessionMetadata, SessionWindow};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::Arc;

const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 50;
/// Bounded safely below SQLite's 30,000-key `IN (...)` limit in `search_session_fts`.
const MAX_ADMITTED_KEYS: usize = 10_000;
const FALLBACK_SNIPPET_MAX_CHARS: usize = 320;

fn sanitize_snippet(raw: &str, max_chars: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(max_chars).collect();
    format!("{truncated}...")
}

/// Search persisted transcripts across sessions strictly owned by `agent_alias`.
///
/// Enforces agent ownership before querying `SessionBackend::search_session_fts`,
/// never accesses `soul.db` or `user_model.db`, and annotates hits with
/// chronology warnings whenever newer messages exist in the matched session.
pub struct SessionSearchTool {
    backend: Arc<dyn SessionBackend>,
    agent_alias: String,
}

impl SessionSearchTool {
    pub fn new(backend: Arc<dyn SessionBackend>, agent_alias: impl Into<String>) -> Self {
        Self {
            backend,
            agent_alias: agent_alias.into(),
        }
    }

    fn collect_admitted_sessions(
        &self,
        requested_session_key: Option<&str>,
    ) -> BTreeMap<String, SessionMetadata> {
        let mut by_key: HashMap<String, SessionMetadata> = self
            .backend
            .list_sessions_with_metadata()
            .into_iter()
            .map(|meta| (meta.key.clone(), meta))
            .collect();

        if let Ok(Some(current_key)) = reiclaw_api::TOOL_LOOP_SESSION_KEY.try_with(Clone::clone)
            && !by_key.contains_key(&current_key)
            && let Some(meta) = self.backend.get_session_metadata(&current_key)
        {
            by_key.insert(meta.key.clone(), meta);
        }

        if let Some(req_key) = requested_session_key
            && !by_key.contains_key(req_key)
            && let Some(meta) = self.backend.get_session_metadata(req_key)
        {
            by_key.insert(meta.key.clone(), meta);
        }

        let mut admitted: Vec<SessionMetadata> = by_key
            .into_values()
            .filter(|meta| meta.agent_alias.as_deref() == Some(self.agent_alias.as_str()))
            .filter(|meta| requested_session_key.is_none_or(|req| meta.key == req))
            .collect();

        admitted.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then_with(|| a.key.cmp(&b.key))
        });
        admitted.truncate(MAX_ADMITTED_KEYS);

        admitted
            .into_iter()
            .map(|meta| (meta.key.clone(), meta))
            .collect()
    }
}

reiclaw_api::tool_attribution!(
    SessionSearchTool,
    reiclaw_api::attribution::ToolKind::Search
);

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search this agent's own persisted session transcripts by keyword. Returns matching snippets with message_id, timestamp, and chronology warnings when newer turns exist so you verify final outcomes instead of intermediate attempts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to search for in your own persisted session transcripts."
                },
                "session_key": {
                    "type": "string",
                    "description": "Optional session key owned by this agent to narrow the search."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max hits to return (default: 10, max: 50)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if let Err(err) = validate_agent_id(&self.agent_alias) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Invalid agent scope: {err}")),
            });
        }

        let Some(query) = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|q| !q.is_empty())
        else {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Invalid 'query': must be a non-empty string.".into()),
            });
        };

        let requested_session_key = match args.get("session_key") {
            Some(v) if !v.is_null() => {
                let Some(raw) = v.as_str() else {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some("Invalid 'session_key': must be a string.".into()),
                    });
                };
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some("Invalid 'session_key': must not be empty.".into()),
                    });
                }
                Some(trimmed)
            }
            _ => None,
        };

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_SEARCH_LIMIT, |v| {
                (v as usize).clamp(1, MAX_SEARCH_LIMIT)
            });

        let admitted_sessions = self.collect_admitted_sessions(requested_session_key);
        if admitted_sessions.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matching session transcripts found.".into(),
                error: None,
            });
        }

        let key_refs: Vec<&str> = admitted_sessions.keys().map(String::as_str).collect();
        let hits = match self.backend.search_session_fts(&key_refs, query, limit) {
            Ok(mut rows) => {
                // Defense in depth: retain only sessions admitted for this agent.
                rows.retain(|hit| admitted_sessions.contains_key(&hit.session_key));
                rows
            }
            Err(err) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Session search failed: {err}")),
                });
            }
        };

        if hits.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matching session transcripts found.".into(),
                error: None,
            });
        }

        let mut windows_by_session: HashMap<String, Option<SessionWindow>> = HashMap::new();
        let mut rendered_hits = Vec::with_capacity(hits.len());
        let mut any_newer = false;

        for hit in &hits {
            let Some(meta) = admitted_sessions.get(&hit.session_key) else {
                continue;
            };
            let window = windows_by_session
                .entry(hit.session_key.clone())
                .or_insert_with(|| self.backend.load_session_window(&hit.session_key).ok())
                .as_ref();

            let latest_window_id = window.and_then(|w| {
                w.delta_messages
                    .last()
                    .map(|(id, _)| *id)
                    .or_else(|| w.active_checkpoint.as_ref().map(|c| c.up_to_message_id))
            });
            let newer_by_id = match latest_window_id {
                Some(latest_id) => hit.message_id < latest_id,
                None => i64::try_from(meta.message_count).is_ok_and(|count| hit.message_id < count),
            };
            let newer_by_ts = hit.created_at.is_some_and(|ts| meta.last_activity > ts);
            let newer_exists = newer_by_id || newer_by_ts;
            any_newer |= newer_exists;

            let newer_delta_count = window.map_or(0, |w| {
                w.delta_messages
                    .iter()
                    .filter(|(id, _)| *id > hit.message_id)
                    .count()
            });

            let timestamp = hit
                .created_at
                .map_or_else(|| "unknown".to_string(), reiclaw_api::time::format_rfc3339);
            let snippet = hit
                .snippet
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map_or_else(
                    || sanitize_snippet(&hit.content, FALLBACK_SNIPPET_MAX_CHARS),
                    |s| sanitize_snippet(s, FALLBACK_SNIPPET_MAX_CHARS),
                );
            let latest_activity = reiclaw_api::time::format_rfc3339(meta.last_activity);

            let mut entry = String::new();
            let _ = writeln!(
                entry,
                "- session_key={} message_id={} role={} timestamp={} (session_messages={}, latest_activity={})",
                hit.session_key,
                hit.message_id,
                hit.role,
                timestamp,
                meta.message_count,
                latest_activity
            );
            let _ = writeln!(entry, "  snippet: {snippet}");
            if newer_exists {
                let newer_note = if newer_delta_count > 0 {
                    format!(", newer_messages_after_hit={newer_delta_count}")
                } else {
                    String::new()
                };
                let _ = writeln!(
                    entry,
                    "  [CHRONOLOGY WARNING] Newer messages exist in session '{}' (message_count={}, latest_activity={}{newer_note}). Check final verified outcomes via sessions_history or episode_read rather than treating earlier intermediate tool attempts as final.",
                    hit.session_key, meta.message_count, latest_activity
                );
            }
            rendered_hits.push(entry);
        }

        let mut output = format!("Found {} session transcript hit(s):\n", rendered_hits.len());
        if any_newer {
            let _ = writeln!(
                output,
                "[Chronology guardrail] Some hits come from earlier points in a session; inspect later messages before relying on intermediate tool outputs."
            );
        }
        for block in rendered_hits {
            output.push_str(&block);
        }

        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reiclaw_api::model_provider::ChatMessage;
    use reiclaw_infra::session_backend::SessionFtsHit;
    use reiclaw_infra::session_sqlite::SqliteSessionBackend;
    use tempfile::TempDir;

    #[tokio::test]
    async fn session_search_isolates_agents_and_appends_chronology_warning() {
        let tmp = TempDir::new().unwrap();
        let backend = Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());

        backend
            .append(
                "sess-alpha",
                &ChatMessage::user("Investigate gateway timeout"),
            )
            .unwrap();
        backend
            .append(
                "sess-alpha",
                &ChatMessage::assistant("Initial deploy attempt failed with timeout"),
            )
            .unwrap();
        backend
            .append(
                "sess-alpha",
                &ChatMessage::assistant(
                    "Final verified fix: increased keepalive and all tests pass",
                ),
            )
            .unwrap();
        backend
            .set_session_agent_alias("sess-alpha", "agent-a")
            .unwrap();

        backend
            .append(
                "sess-beta",
                &ChatMessage::assistant("SECRET_OTHER_AGENT deploy timeout detail"),
            )
            .unwrap();
        backend
            .set_session_agent_alias("sess-beta", "agent-b")
            .unwrap();

        backend
            .append(
                "sess-unowned",
                &ChatMessage::assistant("unattributed timeout"),
            )
            .unwrap();

        let tool = SessionSearchTool::new(backend.clone(), "agent-a");
        let res = tool
            .execute(json!({ "query": "timeout", "limit": 10 }))
            .await
            .unwrap();

        assert!(res.success, "error: {:?}", res.error);
        assert!(res.output.contains("session_key=sess-alpha"));
        assert!(res.output.contains("message_id="));
        assert!(res.output.contains("role="));
        assert!(res.output.contains("timestamp="));
        assert!(res.output.contains("snippet:"));
        assert!(
            res.output.contains("[CHRONOLOGY WARNING]"),
            "earlier hit in sess-alpha must warn about newer messages: {}",
            res.output
        );
        assert!(
            !res.output.contains("SECRET_OTHER_AGENT"),
            "cross-agent session must never leak: {}",
            res.output
        );
        assert!(
            !res.output.contains("sess-unowned"),
            "unattributed session must not be admitted: {}",
            res.output
        );
    }

    #[tokio::test]
    async fn session_search_latest_message_hit_does_not_false_warn_and_blocks_foreign_session_key()
    {
        let tmp = TempDir::new().unwrap();
        let backend = Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());

        backend
            .append("sess-own", &ChatMessage::user("hello"))
            .unwrap();
        backend
            .append(
                "sess-own",
                &ChatMessage::assistant("Final settled keyword zephyr_unique"),
            )
            .unwrap();
        backend
            .set_session_agent_alias("sess-own", "agent-a")
            .unwrap();

        backend
            .append(
                "sess-foreign",
                &ChatMessage::assistant("Foreign keyword zephyr_unique"),
            )
            .unwrap();
        backend
            .set_session_agent_alias("sess-foreign", "agent-b")
            .unwrap();

        let tool = SessionSearchTool::new(backend, "agent-a");

        // Latest message hit should not emit a per-hit chronology warning.
        let latest = reiclaw_api::TOOL_LOOP_SESSION_KEY
            .scope(
                Some("sess-own".to_string()),
                tool.execute(json!({ "query": "zephyr_unique" })),
            )
            .await
            .unwrap();
        assert!(latest.success);
        assert!(latest.output.contains("session_key=sess-own"));
        assert!(
            !latest.output.contains("[CHRONOLOGY WARNING]"),
            "hit on the very last message has no newer messages: {}",
            latest.output
        );

        // Requesting another agent's session_key directly returns no hits.
        let foreign = reiclaw_api::TOOL_LOOP_SESSION_KEY
            .scope(
                Some("sess-foreign".to_string()),
                tool.execute(json!({ "query": "zephyr_unique", "session_key": "sess-foreign" })),
            )
            .await
            .unwrap();
        assert!(foreign.success);
        assert!(
            !foreign.output.contains("Foreign keyword"),
            "must never admit TOOL_LOOP_SESSION_KEY owned by another agent: {}",
            foreign.output
        );
    }

    struct MockChronologyBackend {
        hits: Vec<SessionFtsHit>,
        metas: Vec<SessionMetadata>,
    }

    impl SessionBackend for MockChronologyBackend {
        fn load(&self, _session_key: &str) -> Vec<ChatMessage> {
            Vec::new()
        }
        fn append(&self, _session_key: &str, _message: &ChatMessage) -> std::io::Result<()> {
            Ok(())
        }
        fn remove_last(&self, _session_key: &str) -> std::io::Result<bool> {
            Ok(false)
        }
        fn list_sessions(&self) -> Vec<String> {
            self.metas.iter().map(|m| m.key.clone()).collect()
        }
        fn list_sessions_with_metadata(&self) -> Vec<SessionMetadata> {
            self.metas.clone()
        }
        fn search_session_fts(
            &self,
            allowed_session_keys: &[&str],
            _query: &str,
            _limit: usize,
        ) -> std::io::Result<Vec<SessionFtsHit>> {
            Ok(self
                .hits
                .iter()
                .filter(|h| allowed_session_keys.contains(&h.session_key.as_str()))
                .cloned()
                .collect())
        }
    }

    #[tokio::test]
    async fn session_search_detects_newer_messages_from_metadata_count_and_timestamp() {
        let earlier = reiclaw_api::time::parse_rfc3339("2026-11-18T10:00:00Z").unwrap();
        let latest = reiclaw_api::time::parse_rfc3339("2026-11-18T10:05:00Z").unwrap();
        let backend = Arc::new(MockChronologyBackend {
            metas: vec![SessionMetadata {
                key: "s1".into(),
                name: None,
                created_at: earlier,
                last_activity: latest,
                message_count: 5,
                agent_alias: Some("agent-a".into()),
                channel_id: None,
                room_id: None,
                sender_id: None,
            }],
            hits: vec![SessionFtsHit {
                session_key: "s1".into(),
                message_id: 2,
                role: "assistant".into(),
                content: "intermediate attempt".into(),
                created_at: Some(earlier),
                snippet: Some("intermediate attempt".into()),
            }],
        });

        let tool = SessionSearchTool::new(backend, "agent-a");
        let res = tool.execute(json!({ "query": "attempt" })).await.unwrap();
        assert!(res.success);
        assert!(
            res.output
                .contains(&reiclaw_api::time::format_rfc3339(earlier)),
            "expected RFC 3339 timestamp in output: {}",
            res.output
        );
        assert!(res.output.contains("[CHRONOLOGY WARNING]"));
    }
}
