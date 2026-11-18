//! Agent-scoped sleep memory episode and daily digest drill-down tool (ADR-025).

use async_trait::async_trait;
use reiclaw_api::memory_traits::MemoryReadPolicy;
use reiclaw_api::sleep_records::{
    DailyDigest, EpisodeRecord, EpisodeSourceKind, validate_agent_id, validate_non_soul_key,
};
use reiclaw_api::tool::{Tool, ToolOutput, ToolResult};
use reiclaw_memory::Memory;
use serde_json::json;
use std::fmt::Write;
use std::sync::{Arc, OnceLock};

const DEFAULT_EPISODE_LIMIT: usize = 5;
const MAX_EPISODE_LIMIT: usize = 25;
const MAX_DAILY_DIGEST_LIMIT: usize = 5;

fn tool_text(key: &str) -> String {
    crate::i18n::get_required_tool_string(key)
}

fn source_kind_label(kind: EpisodeSourceKind) -> &'static str {
    match kind {
        EpisodeSourceKind::Dialogue => "Dialogue",
        EpisodeSourceKind::DelegationDigest => "DelegationDigest",
        EpisodeSourceKind::IdleFlush => "IdleFlush",
        EpisodeSourceKind::StartupRecovery => "StartupRecovery",
    }
}

fn format_list_field(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "  {label}:");
    for item in items {
        let _ = writeln!(out, "    - {item}");
    }
}

fn format_episode(ep: &EpisodeRecord) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "- episode_id={} source_kind={} session_key={} window={}..{}",
        ep.episode_id,
        source_kind_label(ep.source_kind),
        ep.session_key,
        ep.started_at,
        ep.ended_at
    );
    if let Some(msg_id) = ep.up_to_message_id {
        let _ = writeln!(out, "  up_to_message_id: {msg_id}");
    }
    let _ = writeln!(out, "  summary: {}", ep.summary);
    format_list_field(&mut out, "why_and_tradeoffs", &ep.why_and_tradeoffs);
    format_list_field(&mut out, "settled_decisions", &ep.settled_decisions);
    format_list_field(&mut out, "rejected_options", &ep.rejected_options);
    format_list_field(&mut out, "entities", &ep.entities);
    out
}

fn format_daily_digest(digest: &DailyDigest) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "- day_key={} window={}..{}",
        digest.day_key, digest.window_start, digest.window_end
    );
    let _ = writeln!(out, "  narrative: {}", digest.narrative);
    format_list_field(&mut out, "settled_decisions", &digest.settled_decisions);
    format_list_field(&mut out, "why_and_tradeoffs", &digest.why_and_tradeoffs);
    format_list_field(&mut out, "latent_insights", &digest.latent_insights);
    format_list_field(&mut out, "entities", &digest.entities);
    out
}

/// Read consolidated sleep-memory episodes (and optionally recent daily digests)
/// strictly scoped to the calling `agent_alias`.
pub struct EpisodeReadTool {
    memory: Arc<dyn Memory>,
    agent_alias: String,
}

impl EpisodeReadTool {
    pub fn new(memory: Arc<dyn Memory>, agent_alias: impl Into<String>) -> Self {
        Self {
            memory,
            agent_alias: agent_alias.into(),
        }
    }
}

reiclaw_api::tool_attribution!(EpisodeReadTool, reiclaw_api::attribution::ToolKind::Memory);

#[async_trait]
impl Tool for EpisodeReadTool {
    fn name(&self) -> &str {
        "episode_read"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: OnceLock<String> = OnceLock::new();
        DESCRIPTION.get_or_init(|| tool_text("tool-episode-read"))
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "since": {
                    "type": "string",
                    "description": tool_text("tool-episode-read-param-since")
                },
                "until": {
                    "type": "string",
                    "description": tool_text("tool-episode-read-param-until")
                },
                "session_id": {
                    "type": "string",
                    "description": tool_text("tool-episode-read-param-session-id")
                },
                "limit": {
                    "type": "integer",
                    "description": tool_text("tool-episode-read-param-limit")
                },
                "include_daily_digests": {
                    "type": "boolean",
                    "description": tool_text("tool-episode-read-param-include-daily-digests")
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let policy = reiclaw_api::TOOL_LOOP_MEMORY_READ_POLICY
            .try_with(|policy| *policy)
            .unwrap_or_default();
        if policy == MemoryReadPolicy::Disabled {
            return Ok(ToolResult {
                success: true,
                output: "No episodes found.".into(),
                error: None,
            });
        }

        if let Err(err) = validate_agent_id(&self.agent_alias) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Invalid agent scope: {err}")),
            });
        }

        let since = match args.get("since") {
            Some(v) if !v.is_null() => {
                let Some(s) = v.as_str().map(str::trim) else {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some("Invalid 'since': must be an RFC 3339 string.".into()),
                    });
                };
                if reiclaw_api::time::parse_rfc3339(s).is_err() {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!(
                            "Invalid 'since' timestamp '{s}': expected RFC 3339 format."
                        )),
                    });
                }
                Some(s)
            }
            _ => None,
        };

        let until = match args.get("until") {
            Some(v) if !v.is_null() => {
                let Some(u) = v.as_str().map(str::trim) else {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some("Invalid 'until': must be an RFC 3339 string.".into()),
                    });
                };
                if reiclaw_api::time::parse_rfc3339(u).is_err() {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!(
                            "Invalid 'until' timestamp '{u}': expected RFC 3339 format."
                        )),
                    });
                }
                Some(u)
            }
            _ => None,
        };

        if let (Some(s), Some(u)) = (since, until)
            && let (Ok(s_dt), Ok(u_dt)) = (
                reiclaw_api::time::parse_rfc3339(s),
                reiclaw_api::time::parse_rfc3339(u),
            )
            && s_dt > u_dt
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'since' must not be after 'until'.".into()),
            });
        }

        let session_id = match args.get("session_id") {
            Some(v) if !v.is_null() => {
                let Some(sid) = v.as_str().map(str::trim) else {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some("Invalid 'session_id': must be a string.".into()),
                    });
                };
                if let Err(err) = validate_non_soul_key(sid, "session_id") {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!("Invalid 'session_id': {err}")),
                    });
                }
                Some(sid)
            }
            _ => None,
        };

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_EPISODE_LIMIT, |v| {
                (v as usize).clamp(1, MAX_EPISODE_LIMIT)
            });

        let include_daily_digests = args
            .get("include_daily_digests")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let episodes = match self
            .memory
            .read_episodes_in_window(&self.agent_alias, since, until, session_id, limit)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Failed to read episodes: {err}")),
                });
            }
        };

        let digests = if include_daily_digests {
            match self
                .memory
                .load_daily_digests(&self.agent_alias, limit.min(MAX_DAILY_DIGEST_LIMIT))
                .await
            {
                Ok(rows) => rows,
                Err(err) => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!("Failed to load daily digests: {err}")),
                    });
                }
            }
        } else {
            Vec::new()
        };

        if episodes.is_empty() && digests.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No episodes found. Note: superseded episodes are excluded.".into(),
                error: None,
            });
        }

        let mut output = String::new();
        if !episodes.is_empty() {
            let _ = writeln!(output, "Found {} episode(s):", episodes.len());
            for ep in &episodes {
                output.push_str(&format_episode(ep));
            }
        } else {
            let _ = writeln!(output, "Found 0 episode(s).");
        }

        if !digests.is_empty() {
            let _ = writeln!(
                output,
                "\n[Daily digests] ({} summary/summaries):",
                digests.len()
            );
            for digest in &digests {
                output.push_str(&format_daily_digest(digest));
            }
        }

        let _ = writeln!(
            output,
            "\nNote: Superseded episodes are excluded from these results; do not mistake early attempts in older episodes for settled outcomes if newer turns or episodes exist."
        );

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
    use reiclaw_api::memory_traits::{MemoryCategory, MemoryEntry};
    use std::collections::BTreeMap;

    struct MockSleepMemory {
        episodes: Vec<EpisodeRecord>,
        digests: Vec<DailyDigest>,
    }

    #[async_trait]
    impl Memory for MockSleepMemory {
        fn name(&self) -> &str {
            "mock-sleep"
        }
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
        async fn store_with_agent(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn read_episodes_in_window(
            &self,
            agent_id: &str,
            _since: Option<&str>,
            _until: Option<&str>,
            session_id: Option<&str>,
            limit: usize,
        ) -> anyhow::Result<Vec<EpisodeRecord>> {
            Ok(self
                .episodes
                .iter()
                .filter(|e| e.agent_id == agent_id)
                .filter(|e| session_id.is_none_or(|sid| e.session_key == sid))
                .take(limit)
                .cloned()
                .collect())
        }
        async fn load_daily_digests(
            &self,
            agent_id: &str,
            limit: usize,
        ) -> anyhow::Result<Vec<DailyDigest>> {
            Ok(self
                .digests
                .iter()
                .filter(|d| d.agent_id == agent_id)
                .take(limit)
                .cloned()
                .collect())
        }
    }

    impl reiclaw_api::attribution::Attributable for MockSleepMemory {
        fn role(&self) -> reiclaw_api::attribution::Role {
            reiclaw_api::attribution::Role::Memory(reiclaw_api::attribution::MemoryKind::InMemory)
        }
        fn alias(&self) -> &str {
            "mock-sleep"
        }
    }

    fn sample_episode(agent_id: &str, kind: EpisodeSourceKind) -> EpisodeRecord {
        EpisodeRecord {
            episode_id: "ep-1".into(),
            agent_id: agent_id.into(),
            session_key: "sess-1".into(),
            started_at: "2026-11-18T08:00:00Z".into(),
            ended_at: "2026-11-18T08:30:00Z".into(),
            up_to_message_id: Some(42),
            summary: "Migrated foreground stack to trait boundary".into(),
            why_and_tradeoffs: vec!["Avoid second SQLite handle".into()],
            settled_decisions: vec!["Keep L0 frozen for 300s".into()],
            rejected_options: vec!["Inline dynamic recall into system prompt".into()],
            entities: vec!["PinnedDigest".into(), "SessionCheckpoint".into()],
            source_kind: kind,
        }
    }

    #[tokio::test]
    async fn episode_read_formats_fields_digests_and_supersession_note() {
        let mem = Arc::new(MockSleepMemory {
            episodes: vec![
                sample_episode("agent-a", EpisodeSourceKind::DelegationDigest),
                sample_episode("agent-b", EpisodeSourceKind::Dialogue),
            ],
            digests: vec![DailyDigest {
                day_key: "2026-11-18".into(),
                agent_id: "agent-a".into(),
                window_start: "2026-11-18T00:00:00Z".into(),
                window_end: "2026-11-18T23:59:59Z".into(),
                narrative: "Completed Wave 2 context stack integration.".into(),
                settled_decisions: vec!["Use L0-L3 foreground context stack".into()],
                why_and_tradeoffs: vec!["Cache prefix stability".into()],
                latent_insights: vec![],
                action_candidates: vec![],
                entities: vec!["zeroclaw-runtime".into()],
                created_at: "2026-11-18T23:59:59Z".into(),
                owner_edited: false,
                action_decisions: BTreeMap::new(),
            }],
        });

        let tool = EpisodeReadTool::new(mem, "agent-a");
        let res = tool
            .execute(json!({ "include_daily_digests": true }))
            .await
            .unwrap();

        assert!(res.success, "error: {:?}", res.error);
        assert!(res.output.contains("episode_id=ep-1"));
        assert!(res.output.contains("source_kind=DelegationDigest"));
        assert!(res.output.contains("why_and_tradeoffs:"));
        assert!(res.output.contains("settled_decisions:"));
        assert!(res.output.contains("rejected_options:"));
        assert!(res.output.contains("entities:"));
        assert!(res.output.contains("[Daily digests]"));
        assert!(res.output.contains("day_key=2026-11-18"));
        assert!(
            res.output.contains("Superseded episodes are excluded"),
            "must remind model about supersession & chronology: {}",
            res.output
        );
    }

    #[tokio::test]
    async fn episode_read_respects_disabled_policy_and_validates_timestamps() {
        let mem = Arc::new(MockSleepMemory {
            episodes: vec![sample_episode("agent-a", EpisodeSourceKind::IdleFlush)],
            digests: vec![],
        });
        let tool = EpisodeReadTool::new(mem, "agent-a");

        let disabled = reiclaw_api::TOOL_LOOP_MEMORY_READ_POLICY
            .scope(MemoryReadPolicy::Disabled, tool.execute(json!({})))
            .await
            .unwrap();
        assert!(disabled.success);
        assert!(!disabled.output.contains("ep-1"));

        let bad_ts = tool
            .execute(json!({ "since": "not-a-date" }))
            .await
            .unwrap();
        assert!(!bad_ts.success);

        let inverted = tool
            .execute(json!({
                "since": "2026-11-18T12:00:00Z",
                "until": "2026-11-18T08:00:00Z"
            }))
            .await
            .unwrap();
        assert!(!inverted.success);
    }
}
