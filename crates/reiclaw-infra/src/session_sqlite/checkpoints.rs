//! Double-buffered session checkpoints and scoped FTS5 search (ADR-025).

use super::SqliteSessionBackend;
use crate::session_backend::{
    CheckpointSlot, SessionCheckpoint, SessionFtsHit, SessionWindow, TimestampedMessage,
};
use reiclaw_api::model_provider::ChatMessage;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::io;

/// Maximum number of allowed session keys accepted in a single parameterized
/// FTS5 `IN (...)` clause (safely below SQLite's 32,766 host-parameter limit).
const MAX_ALLOWED_FTS_KEYS: usize = 30_000;

/// Initialize the `session_checkpoints` schema inside the caller's transaction.
pub(super) fn initialize(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_checkpoints (
            session_key TEXT NOT NULL,
            slot TEXT NOT NULL CHECK (slot IN ('active', 'staged')),
            up_to_message_id INTEGER NOT NULL,
            digest TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (session_key, slot)
        );",
    )
}

/// Remove all checkpoints belonging to `session_key` inside the caller's transaction.
pub(super) fn delete_for_session_tx(
    tx: &rusqlite::Transaction<'_>,
    session_key: &str,
) -> io::Result<()> {
    tx.execute(
        "DELETE FROM session_checkpoints WHERE session_key = ?1",
        params![session_key],
    )
    .map_err(io::Error::other)?;
    Ok(())
}

fn parse_slot(raw: &str) -> io::Result<CheckpointSlot> {
    match raw {
        "active" => Ok(CheckpointSlot::Active),
        "staged" => Ok(CheckpointSlot::Staged),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown checkpoint slot: {other}"),
        )),
    }
}

fn parse_checkpoint_timestamp(raw: &str) -> io::Result<jiff::Timestamp> {
    reiclaw_api::time::parse_rfc3339(raw)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Quote and sanitize search terms into a column-scoped FTS5 `OR` expression.
/// Returns `None` when no token contains alphanumeric content.
fn build_fts5_content_query(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|raw| raw.chars().any(char::is_alphanumeric))
        .map(|raw| {
            let sanitized = raw.replace('\0', "").replace('"', "\"\"");
            format!("content : \"{sanitized}\"")
        })
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

impl SqliteSessionBackend {
    pub(super) fn stage_checkpoint_impl(
        &self,
        session_key: &str,
        up_to_message_id: i64,
        digest: &str,
    ) -> io::Result<SessionCheckpoint> {
        if session_key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session_key must not be empty",
            ));
        }
        if up_to_message_id <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "up_to_message_id must be positive",
            ));
        }
        if digest.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint digest must not be empty",
            ));
        }

        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;

        let row_exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1 AND session_key = ?2)",
                params![up_to_message_id, session_key],
                |row| row.get(0),
            )
            .map_err(io::Error::other)?;
        if !row_exists {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint watermark does not match a persisted row in this session",
            ));
        }

        let now = jiff::Timestamp::now();
        let now_str = reiclaw_api::time::format_rfc3339(now);
        tx.execute(
            "INSERT INTO session_checkpoints
                (session_key, slot, up_to_message_id, digest, created_at, updated_at)
             VALUES (?1, 'staged', ?2, ?3, ?4, ?4)
             ON CONFLICT(session_key, slot) DO UPDATE SET
                up_to_message_id = excluded.up_to_message_id,
                digest = excluded.digest,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at",
            params![session_key, up_to_message_id, digest, now_str],
        )
        .map_err(io::Error::other)?;

        tx.commit().map_err(io::Error::other)?;
        Ok(SessionCheckpoint {
            session_key: session_key.to_string(),
            slot: CheckpointSlot::Staged,
            up_to_message_id,
            digest: digest.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    pub(super) fn promote_staged_checkpoint_impl(
        &self,
        session_key: &str,
    ) -> io::Result<Option<SessionCheckpoint>> {
        if session_key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session_key must not be empty",
            ));
        }

        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;

        let staged = tx
            .query_row(
                "SELECT up_to_message_id, digest, created_at
                 FROM session_checkpoints
                 WHERE session_key = ?1 AND slot = 'staged'",
                params![session_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(io::Error::other)?;

        let Some((staged_watermark, digest, staged_created_str)) = staged else {
            return Ok(None);
        };

        if staged_watermark <= 0 || digest.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged checkpoint failed integrity validation",
            ));
        }

        let active_existing = tx
            .query_row(
                "SELECT up_to_message_id, created_at
                 FROM session_checkpoints
                 WHERE session_key = ?1 AND slot = 'active'",
                params![session_key],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(io::Error::other)?;

        if let Some((active_watermark, _)) = &active_existing
            && staged_watermark < *active_watermark
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staged checkpoint watermark precedes active checkpoint watermark",
            ));
        }

        let active_created_str = active_existing
            .map(|(_, created)| created)
            .unwrap_or(staged_created_str);
        let created_at = parse_checkpoint_timestamp(&active_created_str)?;
        let updated_at = jiff::Timestamp::now();
        let updated_str = reiclaw_api::time::format_rfc3339(updated_at);

        tx.execute(
            "INSERT INTO session_checkpoints
                (session_key, slot, up_to_message_id, digest, created_at, updated_at)
             VALUES (?1, 'active', ?2, ?3, ?4, ?5)
             ON CONFLICT(session_key, slot) DO UPDATE SET
                up_to_message_id = excluded.up_to_message_id,
                digest = excluded.digest,
                updated_at = excluded.updated_at",
            params![
                session_key,
                staged_watermark,
                digest,
                active_created_str,
                updated_str
            ],
        )
        .map_err(io::Error::other)?;

        tx.execute(
            "DELETE FROM session_checkpoints WHERE session_key = ?1 AND slot = 'staged'",
            params![session_key],
        )
        .map_err(io::Error::other)?;

        tx.commit().map_err(io::Error::other)?;
        Ok(Some(SessionCheckpoint {
            session_key: session_key.to_string(),
            slot: CheckpointSlot::Active,
            up_to_message_id: staged_watermark,
            digest,
            created_at,
            updated_at,
        }))
    }

    pub(super) fn load_session_window_impl(&self, session_key: &str) -> io::Result<SessionWindow> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(io::Error::other)?;

        let mut active_checkpoint = None;
        let mut staged_checkpoint = None;
        {
            let mut stmt = tx
                .prepare(
                    "SELECT slot, up_to_message_id, digest, created_at, updated_at
                     FROM session_checkpoints
                     WHERE session_key = ?1",
                )
                .map_err(io::Error::other)?;
            let rows = stmt
                .query_map(params![session_key], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(io::Error::other)?;

            for row in rows {
                let (slot_raw, up_to_message_id, digest, created_raw, updated_raw) =
                    row.map_err(io::Error::other)?;
                let slot = parse_slot(&slot_raw)?;
                let created_at = parse_checkpoint_timestamp(&created_raw)?;
                let updated_at = parse_checkpoint_timestamp(&updated_raw)?;
                let checkpoint = SessionCheckpoint {
                    session_key: session_key.to_string(),
                    slot,
                    up_to_message_id,
                    digest,
                    created_at,
                    updated_at,
                };
                match slot {
                    CheckpointSlot::Active => active_checkpoint = Some(checkpoint),
                    CheckpointSlot::Staged => staged_checkpoint = Some(checkpoint),
                }
            }
        }

        let active_watermark = active_checkpoint
            .as_ref()
            .map_or(0, |ckpt| ckpt.up_to_message_id);

        let delta_messages = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, role, content, created_at, message_source
                     FROM sessions
                     WHERE session_key = ?1 AND id > ?2
                     ORDER BY id ASC",
                )
                .map_err(io::Error::other)?;
            let rows = stmt
                .query_map(params![session_key, active_watermark], |row| {
                    let id: i64 = row.get(0)?;
                    let role: String = row.get(1)?;
                    let content: String = row.get(2)?;
                    let created_at_raw: Option<String> = row.get(3).ok();
                    let created_at = created_at_raw
                        .as_deref()
                        .and_then(|raw| reiclaw_api::time::parse_rfc3339(raw).ok());
                    let ingress = row
                        .get::<_, Option<String>>(4)?
                        .and_then(|raw| serde_json::from_str(&raw).ok());
                    Ok((
                        id,
                        TimestampedMessage {
                            message: ChatMessage { role, content },
                            created_at,
                            ingress,
                        },
                    ))
                })
                .map_err(io::Error::other)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(io::Error::other)?
        };

        tx.commit().map_err(io::Error::other)?;
        Ok(SessionWindow {
            active_checkpoint,
            staged_checkpoint,
            delta_messages,
        })
    }

    pub(super) fn search_session_fts_impl(
        &self,
        allowed_session_keys: &[&str],
        query: &str,
        limit: usize,
    ) -> io::Result<Vec<SessionFtsHit>> {
        if allowed_session_keys.is_empty() || query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut keys: Vec<&str> = Vec::with_capacity(allowed_session_keys.len());
        for &key in allowed_session_keys {
            if !key.is_empty() && !keys.contains(&key) {
                keys.push(key);
            }
        }
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if keys.len() > MAX_ALLOWED_FTS_KEYS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "allowed_session_keys exceeds SQLite parameter limit",
            ));
        }

        let Some(fts_query) = build_fts5_content_query(query) else {
            return Ok(Vec::new());
        };

        let placeholders: String = (0..keys.len())
            .map(|idx| format!("?{}", idx + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let limit_param = keys.len() + 2;
        let sql = format!(
            "SELECT s.session_key, s.id, s.role, s.content, s.created_at,
                    snippet(sessions_fts, 1, '', '', '...', 24)
             FROM sessions_fts
             JOIN sessions s ON s.id = sessions_fts.rowid
             WHERE sessions_fts MATCH ?1
               AND s.session_key IN ({placeholders})
             ORDER BY bm25(sessions_fts) ASC, s.id DESC
             LIMIT ?{limit_param}"
        );

        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut param_values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(keys.len() + 2);
        param_values.push(&fts_query);
        for key in &keys {
            param_values.push(key);
        }
        param_values.push(&limit_i64);

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql).map_err(io::Error::other)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(param_values), |row| {
                let session_key: String = row.get(0)?;
                let message_id: i64 = row.get(1)?;
                let role: String = row.get(2)?;
                let content: String = row.get(3)?;
                let created_at_raw: Option<String> = row.get(4).ok();
                let created_at = created_at_raw
                    .as_deref()
                    .and_then(|raw| reiclaw_api::time::parse_rfc3339(raw).ok());
                let snippet: Option<String> = row.get(5).ok();
                Ok(SessionFtsHit {
                    session_key,
                    message_id,
                    role,
                    content,
                    created_at,
                    snippet,
                })
            })
            .map_err(io::Error::other)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_backend::SessionBackend;
    use reiclaw_api::review::{UserMessageIngress, UserMessageSource};
    use tempfile::TempDir;

    fn message_ids(backend: &SqliteSessionBackend, session_key: &str) -> Vec<i64> {
        let conn = backend.conn.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM sessions WHERE session_key = ?1 ORDER BY id ASC")
            .unwrap();
        stmt.query_map(params![session_key], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<i64>>>()
            .unwrap()
    }

    fn checkpoint_count(backend: &SqliteSessionBackend, session_key: &str) -> i64 {
        let conn = backend.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM session_checkpoints WHERE session_key = ?1",
            params![session_key],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn stage_checkpoint_rejects_invalid_and_cross_session_watermarks() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend
            .append("user_a", &ChatMessage::user("first turn"))
            .unwrap();
        backend
            .append("user_b", &ChatMessage::user("other session turn"))
            .unwrap();

        let ids_a = message_ids(&backend, "user_a");
        let ids_b = message_ids(&backend, "user_b");
        assert_eq!(ids_a.len(), 1);
        assert_eq!(ids_b.len(), 1);

        // Non-positive watermark, empty digest, and empty session key fail closed.
        assert!(backend.stage_checkpoint("user_a", 0, "summary").is_err());
        assert!(backend.stage_checkpoint("user_a", -5, "summary").is_err());
        assert!(backend.stage_checkpoint("user_a", ids_a[0], "   ").is_err());
        assert!(backend.stage_checkpoint("", ids_a[0], "summary").is_err());

        // Non-existent message id fails closed.
        assert!(
            backend
                .stage_checkpoint("user_a", 99_999, "summary")
                .is_err()
        );

        // Cross-session watermark spoofing fails closed.
        assert!(
            backend
                .stage_checkpoint("user_a", ids_b[0], "spoofed cross-session summary")
                .is_err()
        );
        assert_eq!(checkpoint_count(&backend, "user_a"), 0);

        // Valid watermark in own session succeeds.
        let staged = backend
            .stage_checkpoint("user_a", ids_a[0], "settled decision: keep L1 compact")
            .unwrap();
        assert_eq!(staged.slot, CheckpointSlot::Staged);
        assert_eq!(staged.up_to_message_id, ids_a[0]);
        assert_eq!(checkpoint_count(&backend, "user_a"), 1);
    }

    #[test]
    fn promote_staged_checkpoint_is_atomic_and_monotonic() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        // Promoting without a staged row is a no-op returning None.
        assert!(
            backend
                .promote_staged_checkpoint("zeroclaw_user")
                .unwrap()
                .is_none()
        );

        backend
            .append("zeroclaw_user", &ChatMessage::user("turn 1"))
            .unwrap();
        backend
            .append("zeroclaw_user", &ChatMessage::assistant("reply 1"))
            .unwrap();
        backend
            .append("zeroclaw_user", &ChatMessage::user("turn 2"))
            .unwrap();
        let ids = message_ids(&backend, "zeroclaw_user");
        assert_eq!(ids.len(), 3);

        // Stage watermark at message 2 and promote -> active at 2, staged deleted.
        backend
            .stage_checkpoint("zeroclaw_user", ids[1], "digest up to msg 2")
            .unwrap();
        let active = backend
            .promote_staged_checkpoint("zeroclaw_user")
            .unwrap()
            .expect("staged checkpoint should promote");
        assert_eq!(active.slot, CheckpointSlot::Active);
        assert_eq!(active.up_to_message_id, ids[1]);
        assert_eq!(active.digest, "digest up to msg 2");
        assert_eq!(checkpoint_count(&backend, "zeroclaw_user"), 1);

        // Regressing watermark staged at message 1 must fail closed and leave active=2, staged=1 intact.
        backend
            .stage_checkpoint("zeroclaw_user", ids[0], "stale digest")
            .unwrap();
        assert!(backend.promote_staged_checkpoint("zeroclaw_user").is_err());

        let window = backend.load_session_window("zeroclaw_user").unwrap();
        assert_eq!(
            window
                .active_checkpoint
                .as_ref()
                .map(|c| c.up_to_message_id),
            Some(ids[1])
        );
        assert_eq!(
            window
                .staged_checkpoint
                .as_ref()
                .map(|c| c.up_to_message_id),
            Some(ids[0])
        );

        // Equal watermark succeeds, and advancing watermark to message 3 succeeds.
        backend
            .stage_checkpoint("zeroclaw_user", ids[1], "refined digest at msg 2")
            .unwrap();
        let promoted_eq = backend
            .promote_staged_checkpoint("zeroclaw_user")
            .unwrap()
            .unwrap();
        assert_eq!(promoted_eq.up_to_message_id, ids[1]);
        assert_eq!(promoted_eq.digest, "refined digest at msg 2");

        backend
            .stage_checkpoint("zeroclaw_user", ids[2], "digest up to msg 3")
            .unwrap();
        let promoted_fwd = backend
            .promote_staged_checkpoint("zeroclaw_user")
            .unwrap()
            .unwrap();
        assert_eq!(promoted_fwd.up_to_message_id, ids[2]);
        assert_eq!(checkpoint_count(&backend, "zeroclaw_user"), 1);
    }

    #[test]
    fn load_session_window_returns_delta_messages_and_preserves_ingress() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        let ingress1 = UserMessageIngress {
            source: UserMessageSource::Operator,
            text: "raw turn 1".to_string(),
        };
        let ingress2 = UserMessageIngress {
            source: UserMessageSource::Channel {
                sender_id: "zeroclaw_operator".to_string(),
            },
            text: "raw turn 2".to_string(),
        };

        backend
            .append_with_ingress("zeroclaw_user", &ChatMessage::user("enriched 1"), &ingress1)
            .unwrap();
        backend
            .append("zeroclaw_user", &ChatMessage::assistant("reply 1"))
            .unwrap();
        backend
            .append_with_ingress("zeroclaw_user", &ChatMessage::user("enriched 2"), &ingress2)
            .unwrap();
        backend
            .append("zeroclaw_user", &ChatMessage::assistant("reply 2"))
            .unwrap();

        let ids = message_ids(&backend, "zeroclaw_user");
        assert_eq!(ids.len(), 4);

        // Before any checkpoint, all 4 messages are in delta_messages.
        let initial = backend.load_session_window("zeroclaw_user").unwrap();
        assert!(initial.active_checkpoint.is_none());
        assert!(initial.staged_checkpoint.is_none());
        assert_eq!(initial.delta_messages.len(), 4);

        // Promote active checkpoint to msg 2, and stage another at msg 3.
        backend
            .stage_checkpoint("zeroclaw_user", ids[1], "active digest through reply 1")
            .unwrap();
        backend.promote_staged_checkpoint("zeroclaw_user").unwrap();
        backend
            .stage_checkpoint("zeroclaw_user", ids[2], "staged digest through turn 2")
            .unwrap();

        let window = backend.load_session_window("zeroclaw_user").unwrap();
        assert_eq!(
            window
                .active_checkpoint
                .as_ref()
                .map(|c| c.up_to_message_id),
            Some(ids[1])
        );
        assert_eq!(
            window
                .staged_checkpoint
                .as_ref()
                .map(|c| c.up_to_message_id),
            Some(ids[2])
        );
        // Active watermark is ids[1], so delta must contain strictly ids[2] and ids[3].
        assert_eq!(window.delta_messages.len(), 2);
        assert_eq!(window.delta_messages[0].0, ids[2]);
        assert_eq!(window.delta_messages[0].1.message.content, "enriched 2");
        assert_eq!(window.delta_messages[0].1.ingress.as_ref(), Some(&ingress2));
        assert!(window.delta_messages[0].1.created_at.is_some());
        assert_eq!(window.delta_messages[1].0, ids[3]);
        assert_eq!(window.delta_messages[1].1.message.content, "reply 2");
        assert!(window.delta_messages[1].1.ingress.is_none());
    }

    #[test]
    fn search_session_fts_handles_punctuation_and_enforces_scope_before_limit() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend
            .append(
                "user_a",
                &ChatMessage::user("Rust compiler error: [E0308] mismatched types in checkpoint"),
            )
            .unwrap();
        backend
            .append(
                "user_b",
                &ChatMessage::user("checkpoint checkpoint checkpoint E0308 private note"),
            )
            .unwrap();
        backend
            .append(
                "user_b",
                &ChatMessage::assistant("another checkpoint E0308 note in user_b"),
            )
            .unwrap();

        // Guard conditions return empty without hitting FTS5.
        assert!(
            backend
                .search_session_fts(&[], "checkpoint", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            backend
                .search_session_fts(&["user_a"], "   ", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            backend
                .search_session_fts(&["user_a"], "checkpoint", 0)
                .unwrap()
                .is_empty()
        );

        // Punctuation-only and operator-laden queries never raise FTS5 syntax errors.
        assert!(
            backend
                .search_session_fts(&["user_a"], "--- ??? () \"\" * ^", 10)
                .unwrap()
                .is_empty()
        );
        let hits_punct = backend
            .search_session_fts(
                &["user_a"],
                "error: [E0308] \"checkpoint\" AND OR NOT () ---",
                10,
            )
            .unwrap();
        assert_eq!(hits_punct.len(), 1);
        assert_eq!(hits_punct[0].session_key, "user_a");
        assert!(hits_punct[0].snippet.is_some());

        // Strict filtering BEFORE LIMIT: even with limit=1 and higher-scoring user_b rows,
        // scoping to user_a must still return user_a's hit and never leak user_b.
        let scoped = backend
            .search_session_fts(&["user_a"], "checkpoint", 1)
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session_key, "user_a");

        // Column scoping: searching for session_key literal does not match content if absent.
        let key_only = backend
            .search_session_fts(&["user_a"], "user_a", 10)
            .unwrap();
        assert!(key_only.is_empty());
    }

    #[test]
    fn clear_delete_and_cleanup_stale_cascade_to_checkpoints() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        for key in ["sess_clear", "sess_delete", "sess_stale", "sess_keep"] {
            backend.append(key, &ChatMessage::user("hello")).unwrap();
            let ids = message_ids(&backend, key);
            backend
                .stage_checkpoint(key, ids[0], "staged digest")
                .unwrap();
            backend.promote_staged_checkpoint(key).unwrap();
            backend
                .stage_checkpoint(key, ids[0], "next staged digest")
                .unwrap();
            assert_eq!(checkpoint_count(&backend, key), 2);
        }

        // clear_messages cascades to session_checkpoints for that session only.
        assert_eq!(backend.clear_messages("sess_clear").unwrap(), 1);
        assert_eq!(checkpoint_count(&backend, "sess_clear"), 0);
        assert_eq!(checkpoint_count(&backend, "sess_keep"), 2);

        // delete_session cascades to session_checkpoints for that session only.
        assert!(backend.delete_session("sess_delete").unwrap());
        assert_eq!(checkpoint_count(&backend, "sess_delete"), 0);
        assert_eq!(checkpoint_count(&backend, "sess_keep"), 2);

        // cleanup_stale cascades to session_checkpoints for expired sessions only.
        let old_ts = reiclaw_api::time::format_rfc3339(
            jiff::Timestamp::now()
                .checked_sub(jiff::SignedDuration::from_hours(48))
                .unwrap(),
        );
        {
            let conn = backend.conn.lock();
            conn.execute(
                "UPDATE session_metadata SET last_activity = ?1 WHERE session_key = 'sess_stale'",
                params![old_ts],
            )
            .unwrap();
        }
        let cleaned = backend.cleanup_stale(24).unwrap();
        assert_eq!(cleaned, 1);
        assert_eq!(checkpoint_count(&backend, "sess_stale"), 0);
        assert_eq!(checkpoint_count(&backend, "sess_keep"), 2);
    }
}
