//! Source grants belong to individual admitted requests, never to a dispatch's
//! shared continuation budget. Session lifecycle revokes facts, not task history.

use super::{DispatchBindingRow, SqliteSessionBackend, checkpoints};
use crate::session_backend::SessionFtsHit;
use anyhow::{Context, Result, ensure};
use reiclaw_api::delegation_request::{DelegationOrigin, DelegationOriginScope};
use rusqlite::{Connection, params};

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for (column, declaration) in [
        ("origin_context", "TEXT"),
        ("origin_revoked", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = ?1",
            [column],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute(
                &format!("ALTER TABLE session_delegations ADD COLUMN {column} {declaration}"),
                [],
            )?;
        }
    }
    // The immutable JSON remains the one source of origin facts. The expression
    // index supports lifecycle revocation without duplicating the source key.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_session_delegations_origin
         ON session_delegations(json_extract(origin_context, '$.session_key'));
         CREATE TRIGGER IF NOT EXISTS delegation_origin_session_deleted
         AFTER DELETE ON session_metadata BEGIN
           UPDATE session_delegations SET origin_revoked = 1
           WHERE json_extract(origin_context, '$.session_key') = OLD.session_key;
         END;
         CREATE TRIGGER IF NOT EXISTS delegation_origin_owner_changed
         AFTER UPDATE OF agent_alias ON session_metadata
         WHEN OLD.agent_alias IS NOT NEW.agent_alias BEGIN
           UPDATE session_delegations SET origin_revoked = 1
           WHERE json_extract(origin_context, '$.session_key') = OLD.session_key;
         END;",
    )
    .context("migrating delegation source grants")?;
    Ok(())
}

pub(super) fn revoke_for_session(conn: &Connection, session_key: &str) -> Result<()> {
    conn.execute(
        "UPDATE session_delegations SET origin_revoked = 1
         WHERE json_extract(origin_context, '$.session_key') = ?1",
        [session_key],
    )?;
    Ok(())
}

pub(super) fn validate_admission(
    conn: &Connection,
    agent_alias: &str,
    origin: &DelegationOrigin,
) -> Result<()> {
    ensure!(
        !origin.session_key.trim().is_empty() && origin.session_key.len() <= 512,
        "invalid delegation source session"
    );
    if let DelegationOriginScope::SessionFacts {
        operator_subject,
        agent_id,
        memory_session_id,
    } = &origin.scope
    {
        ensure!(
            operator_subject.len() == 64
                && operator_subject
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && !agent_id.trim().is_empty()
                && agent_id.len() <= 256
                && memory_session_id
                    .as_ref()
                    .is_none_or(|id| !id.trim().is_empty() && id.len() <= 512),
            "invalid delegation source authority"
        );
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_metadata WHERE session_key = ?1 AND agent_alias = ?2)",
            params![origin.session_key, agent_alias],
            |row| row.get(0),
        )?;
        ensure!(
            exists,
            "delegation source session is missing or belongs to another agent"
        );
    }
    Ok(())
}

/// Used under the caller's transaction for both reads and attempt reservation.
pub(super) fn is_current(conn: &Connection, binding: &DispatchBindingRow) -> Result<bool> {
    let Some(origin) = binding.origin.as_ref() else {
        return Ok(false);
    };
    if !matches!(origin.scope, DelegationOriginScope::SessionFacts { .. }) {
        return Ok(false);
    }
    let origin_json = serde_json::to_string(origin)?;
    conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM session_delegations d JOIN session_metadata s
             ON s.session_key = json_extract(d.origin_context, '$.session_key')
           WHERE d.rowid = ?1 AND d.agent_alias = ?2 AND d.request_id = ?3
             AND d.dispatch_id = ?4 AND d.route_digest = ?5
             AND d.origin_context = ?6 AND d.origin_revoked = 0
             AND s.agent_alias = d.agent_alias
         )",
        params![
            binding.rowid,
            binding.agent_alias,
            binding.request_id,
            binding.dispatch_id,
            binding.route_digest,
            origin_json
        ],
        |row| row.get(0),
    )
    .context("checking current delegation source authority")
}

/// Bounded facts read in the same snapshot as the grant and source ownership.
pub struct DelegationSessionFacts {
    pub checkpoint_digests: Vec<String>,
    pub transcript_hits: Vec<SessionFtsHit>,
}

impl SqliteSessionBackend {
    pub fn delegation_origin_is_current(&self, binding: &DispatchBindingRow) -> Result<bool> {
        is_current(&self.conn.lock(), binding)
    }

    /// Missing, revoked or replaced sources yield no data, including after a
    /// session key is reused. No agent-wide history fallback is permitted.
    pub fn delegation_session_facts(
        &self,
        binding: &DispatchBindingRow,
        query: &str,
    ) -> Result<Option<DelegationSessionFacts>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        if !is_current(&tx, binding)? {
            return Ok(None);
        }
        let origin = binding
            .origin
            .as_ref()
            .context("missing delegation source")?;
        let checkpoint_digests = tx
            .prepare(
                "SELECT digest FROM session_checkpoints
             WHERE session_key = ?1 AND format_version = ?2 ORDER BY slot LIMIT 2",
            )?
            .query_map(
                params![
                    origin.session_key,
                    checkpoints::CURRENT_CHECKPOINT_FORMAT_VERSION
                ],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        let transcript_hits =
            checkpoints::search_session_fts_on(&tx, &[&origin.session_key], query, 5)?;
        tx.commit()?;
        Ok(Some(DelegationSessionFacts {
            checkpoint_digests,
            transcript_hits,
        }))
    }
}
