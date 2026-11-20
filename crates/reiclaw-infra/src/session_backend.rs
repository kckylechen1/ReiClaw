//! Trait abstraction for session persistence backends.
use reiclaw_api::bridge_intake::{BridgeInput, BridgeReceipt, BridgeResume, BridgeSource};
use reiclaw_api::model_provider::ChatMessage;

/// Minimum retry protection shared by durable and in-memory receipts. Longer
/// than the Gateway's attachment handle lifetime; state changes do not renew it.
pub const REQUEST_RECEIPT_PROTECTION_MINUTES: u16 = 16;

/// Metadata about a persisted session.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    /// Session key (e.g. `telegram_user123`).
    pub key: String,
    /// Optional human-readable name (e.g. `eyrie-commander-briefing`).
    pub name: Option<String>,
    /// When the session was first created.
    pub created_at: jiff::Timestamp,
    /// When the last message was appended.
    pub last_activity: jiff::Timestamp,
    /// Total number of messages in the session.
    pub message_count: usize,
    /// Alias of the agent that owned this session (HashMap key in
    /// `config.agents`). `None` for sessions persisted before per-agent
    /// attribution landed, or for backends that don't track it.
    pub agent_alias: Option<String>,
    /// Dotted ChannelRef the session belongs to (`<type>.<alias>`,
    /// e.g. `telegram.clamps`). `None` for non-channel sessions (CLI,
    /// internal cron runs) or backends without routing columns.
    pub channel_id: Option<String>,
    /// Platform-side room / thread identifier (chat id,
    /// Matrix room id, Slack thread ts, ...). `None` for direct messages
    /// or backends that don't track it.
    pub room_id: Option<String>,
    /// Inbound sender id verbatim (username, phone number, ...).
    /// Not an FK — sessions can survive deletion of the upstream user.
    pub sender_id: Option<String>,
}

/// Structured routing context recorded alongside a session. Mirrors the
/// `ChannelMessage` fields the orchestrator uses to compose
/// `conversation_history_key` so the session row can be queried by
/// channel / room / sender without re-parsing the synthetic key.
#[derive(Debug, Clone, Default)]
pub struct SessionContext<'a> {
    /// `<type>.<alias>` ChannelRef (`telegram.clamps`).
    pub channel_id: Option<&'a str>,
    /// Platform-side room / thread id.
    pub room_id: Option<&'a str>,
    /// Inbound sender id (channel-native username, phone, ...).
    pub sender_id: Option<&'a str>,
}

/// Query parameters for listing sessions.
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    /// Keyword to search in session messages (FTS5 if available).
    pub keyword: Option<String>,
    /// Maximum number of sessions to return.
    pub limit: Option<usize>,
}

/// One persisted message with the optional `created_at` the backend
/// stamped on it. In-memory backends return `None`; SQLite
/// returns the row's `created_at` column.
#[derive(Debug, Clone)]
pub struct TimestampedMessage {
    pub message: ChatMessage,
    pub created_at: Option<jiff::Timestamp>,
    /// Original text and source before runtime enrichment. Missing on legacy or
    /// unsupported history; neither session metadata nor history is a substitute.
    pub ingress: Option<reiclaw_api::review::UserMessageIngress>,
}

/// Slot in the double-buffered L1 checkpoint store (ADR-025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointSlot {
    Active,
    Staged,
}

impl CheckpointSlot {
    /// Canonical database string representation for this slot.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Staged => "staged",
        }
    }
}

/// Persisted L1 rolling session checkpoint bound to a monotonic `sessions.id` watermark.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCheckpoint {
    pub session_key: String,
    pub slot: CheckpointSlot,
    pub up_to_message_id: i64,
    pub digest: String,
    pub created_at: jiff::Timestamp,
    pub updated_at: jiff::Timestamp,
}

/// Atomic snapshot of a session's double-buffered checkpoints and post-watermark delta messages.
#[derive(Debug, Clone)]
pub struct SessionWindow {
    pub active_checkpoint: Option<SessionCheckpoint>,
    pub staged_checkpoint: Option<SessionCheckpoint>,
    pub delta_messages: Vec<(i64, TimestampedMessage)>,
}

/// Scoped FTS5 match over persisted session transcript rows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionFtsHit {
    pub session_key: String,
    pub message_id: i64,
    pub role: String,
    pub content: String,
    pub created_at: Option<jiff::Timestamp>,
    pub snippet: Option<String>,
}

/// Trait for session persistence backends.
/// Implementations must be `Send + Sync` for sharing across async tasks.
pub trait SessionBackend: Send + Sync {
    /// Load all messages for a session. Returns empty vec if session doesn't exist.
    fn load(&self, session_key: &str) -> Vec<ChatMessage>;

    /// Same as `load`, but each row carries its persisted `created_at`
    /// when the backend has one. Default impl falls back to `load`
    /// without timestamps so non-SQLite backends keep working.
    fn load_with_timestamps(&self, session_key: &str) -> Vec<TimestampedMessage> {
        self.load(session_key)
            .into_iter()
            .map(|message| TimestampedMessage {
                message,
                created_at: None,
                ingress: None,
            })
            .collect()
    }

    /// Append a single message to a session.
    fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()>;

    /// Append with the original text and source supplied by trusted ingress. Backends without
    /// source/timestamp storage retain ordinary history but cannot reflect it.
    fn append_with_ingress(
        &self,
        session_key: &str,
        message: &ChatMessage,
        _ingress: &reiclaw_api::review::UserMessageIngress,
    ) -> std::io::Result<()> {
        self.append(session_key, message)
    }

    /// Remove the last message from a session. Returns `true` if a message was removed.
    fn remove_last(&self, session_key: &str) -> std::io::Result<bool>;

    fn update_last(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<bool> {
        if self.remove_last(session_key)? {
            self.append(session_key, message)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all session keys.
    fn list_sessions(&self) -> Vec<String>;

    /// List sessions with metadata.
    fn list_sessions_with_metadata(&self) -> Vec<SessionMetadata> {
        // Default: construct metadata from messages (backends can override for efficiency)
        self.list_sessions()
            .into_iter()
            .map(|key| {
                let messages = self.load(&key);
                SessionMetadata {
                    key,
                    name: None,
                    created_at: jiff::Timestamp::now(),
                    last_activity: jiff::Timestamp::now(),
                    message_count: messages.len(),
                    agent_alias: None,
                    channel_id: None,
                    room_id: None,
                    sender_id: None,
                }
            })
            .collect()
    }

    /// Compact a session file (remove duplicates/corruption). No-op by default.
    fn compact(&self, _session_key: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Remove sessions that haven't been active within the given TTL hours.
    fn cleanup_stale(&self, _ttl_hours: u32) -> std::io::Result<usize> {
        Ok(0)
    }

    /// Search sessions by keyword. Default returns empty (backends with FTS override).
    fn search(&self, _query: &SessionQuery) -> Vec<SessionMetadata> {
        Vec::new()
    }

    fn clear_messages(&self, session_key: &str) -> std::io::Result<usize> {
        let mut count = 0;
        while self.remove_last(session_key)? {
            count += 1;
        }
        Ok(count)
    }

    /// Delete all messages for a session. Returns `true` if the session existed.
    fn delete_session(&self, _session_key: &str) -> std::io::Result<bool> {
        Ok(false)
    }

    fn clear_agent_attribution(&self, _agent_alias: &str) -> std::io::Result<usize> {
        Ok(0)
    }

    fn rename_agent_attribution(&self, _from: &str, _to: &str) -> std::io::Result<usize> {
        Ok(0)
    }

    fn count_agent_attribution(&self, _agent_alias: &str) -> std::io::Result<usize> {
        Ok(0)
    }

    fn session_exists(&self, session_key: &str) -> bool {
        self.get_session_metadata(session_key).is_some()
    }

    /// Set or update the human-readable name for a session.
    fn set_session_name(&self, _session_key: &str, _name: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Get the human-readable name for a session (if set).
    fn get_session_name(&self, _session_key: &str) -> std::io::Result<Option<String>> {
        Ok(None)
    }

    /// Record the agent alias that owns a session. Called on WebSocket
    /// handshake when the alias is known. No-op for backends that don't
    /// track per-agent attribution.
    fn set_session_agent_alias(
        &self,
        _session_key: &str,
        _agent_alias: &str,
    ) -> std::io::Result<()> {
        Ok(())
    }

    /// Get the agent alias associated with a session, if recorded.
    fn get_session_agent_alias(&self, _session_key: &str) -> std::io::Result<Option<String>> {
        Ok(None)
    }

    fn set_session_context(
        &self,
        _session_key: &str,
        _context: SessionContext<'_>,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn get_session_metadata(&self, session_key: &str) -> Option<SessionMetadata> {
        let messages = self.load(session_key);
        if messages.is_empty() {
            return None;
        }
        Some(SessionMetadata {
            key: session_key.to_string(),
            name: self.get_session_name(session_key).ok().flatten(),
            created_at: jiff::Timestamp::now(),
            last_activity: jiff::Timestamp::now(),
            message_count: messages.len(),
            agent_alias: None,
            channel_id: None,
            room_id: None,
            sender_id: None,
        })
    }

    /// Set the session state (e.g. "idle", "running", "error").
    /// `turn_id` identifies the current turn (set when running, cleared on idle).
    fn set_session_state(
        &self,
        _session_key: &str,
        _state: &str,
        _turn_id: Option<&str>,
    ) -> std::io::Result<()> {
        Ok(())
    }

    /// Get the current session state. Returns `None` if the backend doesn't track state.
    fn get_session_state(&self, _session_key: &str) -> std::io::Result<Option<SessionState>> {
        Ok(None)
    }

    /// List sessions currently in "running" state.
    fn list_running_sessions(&self) -> Vec<SessionMetadata> {
        Vec::new()
    }

    /// List sessions stuck in "running" state longer than `threshold_secs`.
    fn list_stuck_sessions(&self, _threshold_secs: u64) -> Vec<SessionMetadata> {
        Vec::new()
    }

    /// Durably record a client request as accepted for `session_key`, in
    /// state `state`. Returns `None` when this backend does not keep request
    /// receipts, so callers must not claim the acceptance is durable.
    fn record_request(
        &self,
        _session_key: &str,
        _request_id: &str,
        _state: &str,
    ) -> std::io::Result<Option<RequestReceipt>> {
        Ok(None)
    }

    /// Update the state of a recorded request. Unknown requests are ignored.
    fn set_request_state(
        &self,
        _session_key: &str,
        _request_id: &str,
        _state: &str,
    ) -> std::io::Result<()> {
        Ok(())
    }

    /// Resume retained bridge intake from the canonical session store. A
    /// backend without durable intake must refuse, never substitute memory.
    fn bridge_resume(&self, _source: &BridgeSource) -> std::io::Result<BridgeResume> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable bridge intake is unsupported",
        ))
    }

    fn bridge_receipt(
        &self,
        _source: &BridgeSource,
        _update_id: i64,
    ) -> std::io::Result<Option<BridgeReceipt>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable bridge intake is unsupported",
        ))
    }

    /// Persist input and advance only its connected source prefix atomically.
    fn bridge_record(
        &self,
        _source: &BridgeSource,
        _input: &BridgeInput,
    ) -> std::io::Result<BridgeReceipt> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable bridge intake is unsupported",
        ))
    }

    /// Claim pending input before entering any effect-producing path. Only a
    /// `true` result permits execution; concurrent claimers cannot both win.
    fn bridge_claim(&self, _source: &BridgeSource, _update_id: i64) -> std::io::Result<bool> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable bridge intake is unsupported",
        ))
    }

    fn bridge_finish(
        &self,
        _source: &BridgeSource,
        _update_id: i64,
        _state: &str,
    ) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable bridge intake is unsupported",
        ))
    }

    /// Stage a rolling L1 checkpoint for `session_key` up to `up_to_message_id`.
    /// Backends without checkpoint support fail closed with `Unsupported`.
    fn stage_checkpoint(
        &self,
        _session_key: &str,
        _up_to_message_id: i64,
        _digest: &str,
    ) -> std::io::Result<SessionCheckpoint> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session checkpoints are unsupported",
        ))
    }

    /// Atomically promote a staged checkpoint to `active` if its watermark is
    /// monotonic with any existing `active` checkpoint.
    fn promote_staged_checkpoint(
        &self,
        _session_key: &str,
    ) -> std::io::Result<Option<SessionCheckpoint>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session checkpoints are unsupported",
        ))
    }

    /// Atomically read active/staged checkpoints and delta messages after the
    /// active watermark. Backends without checkpoints return raw timestamped history.
    fn load_session_window(&self, session_key: &str) -> std::io::Result<SessionWindow> {
        let delta_messages = self
            .load_with_timestamps(session_key)
            .into_iter()
            .enumerate()
            .map(|(idx, msg)| {
                let synthetic_id = i64::try_from(idx).unwrap_or(i64::MAX - 1).saturating_add(1);
                (synthetic_id, msg)
            })
            .collect();
        Ok(SessionWindow {
            active_checkpoint: None,
            staged_checkpoint: None,
            delta_messages,
        })
    }

    /// Scoped FTS5 search restricted to `allowed_session_keys` before `LIMIT`.
    fn search_session_fts(
        &self,
        _allowed_session_keys: &[&str],
        _query: &str,
        _limit: usize,
    ) -> std::io::Result<Vec<SessionFtsHit>> {
        Ok(Vec::new())
    }
}

/// The result of [`SessionBackend::record_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestReceipt {
    /// First time this request was seen; it is now recorded.
    Recorded,
    /// Already recorded; carries its last recorded state.
    Duplicate { state: String },
}

/// Session state information.
#[derive(Debug, Clone)]
pub struct SessionState {
    /// Current state: "idle", "running", or "error".
    pub state: String,
    /// Turn ID of the active or last turn.
    pub turn_id: Option<String>,
    /// When the current state was entered.
    pub turn_started_at: Option<jiff::Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_metadata_is_constructible() {
        let meta = SessionMetadata {
            key: "test".into(),
            name: None,
            created_at: jiff::Timestamp::now(),
            last_activity: jiff::Timestamp::now(),
            message_count: 5,
            agent_alias: None,
            channel_id: None,
            room_id: None,
            sender_id: None,
        };
        assert_eq!(meta.key, "test");
        assert_eq!(meta.message_count, 5);
    }
}
