//! ADR-023 export authority, owned by UserModelStore in user_model.db.
//! Consent receipts never change a fact, its revision, or its applicability.

use super::*;
use rusqlite::{OptionalExtension, params};
pub use zeroclaw_api::companion::{VoiceConsentAction, VoiceConsentReceipt};

/// Provider identifiers are exact, case-sensitive ASCII slugs; never normalized.
pub fn validate_voice_provider(provider: &str) -> Result<(), rusqlite::Error> {
    if provider.is_empty()
        || provider.len() > 64
        || !provider
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || !provider.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(rusqlite::Error::InvalidParameterName(
            "invalid voice provider".into(),
        ));
    }
    Ok(())
}

pub(super) fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    // Additive and idempotent. Old heads acquire no consent. Older binaries
    // ignore this table; no existing revision or Scope is rewritten.
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS user_model_voice_consent_receipts (
            id TEXT PRIMARY KEY,
            head_revision_id TEXT NOT NULL REFERENCES user_model_revisions(id),
            provider TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            action TEXT NOT NULL CHECK (action IN ('grant', 'revoke')),
            reviewer TEXT NOT NULL CHECK (reviewer = 'owner_operator'),
            at_unix INTEGER NOT NULL,
            UNIQUE (head_revision_id, provider, revision)
         );
         CREATE TRIGGER IF NOT EXISTS user_model_voice_consent_no_update
         BEFORE UPDATE ON user_model_voice_consent_receipts BEGIN
            SELECT RAISE(ABORT, 'voice consent receipts are append-only');
         END;
         CREATE TRIGGER IF NOT EXISTS user_model_voice_consent_no_delete
         BEFORE DELETE ON user_model_voice_consent_receipts BEGIN
            SELECT RAISE(ABORT, 'voice consent receipts are append-only');
         END;
         COMMIT;",
    )
}

fn latest(
    conn: &Connection,
    head: &str,
    provider: &str,
) -> Result<Option<VoiceConsentReceipt>, rusqlite::Error> {
    conn.query_row(
        "SELECT id, head_revision_id, provider, revision, action, reviewer, at_unix
         FROM user_model_voice_consent_receipts
         WHERE head_revision_id = ?1 AND provider = ?2 ORDER BY revision DESC LIMIT 1",
        params![head, provider],
        |row| {
            Ok(VoiceConsentReceipt {
                id: row.get(0)?,
                head_revision_id: row.get(1)?,
                provider: row.get(2)?,
                revision: row.get(3)?,
                action: match row.get::<_, String>(4)?.as_str() {
                    "grant" => VoiceConsentAction::Grant,
                    "revoke" => VoiceConsentAction::Revoke,
                    _ => return Err(rusqlite::Error::InvalidQuery),
                },
                reviewer: row.get(5)?,
                at_unix: row.get(6)?,
            })
        },
    )
    .optional()
}

impl UserModelStore {
    pub fn voice_consent(
        &self,
        head: &str,
        provider: &str,
    ) -> Result<Option<VoiceConsentReceipt>, rusqlite::Error> {
        validate_voice_provider(provider)?;
        latest(&self.conn.lock(), head, provider)
    }

    /// Operator-only ingress must precede this call. CAS binds both the fact
    /// shown to the owner and its prior export decision. A stale editor cannot
    /// re-grant a revoked consent. Revoke may also retire a historical head.
    pub fn set_voice_consent(
        &self,
        head: &str,
        provider: &str,
        action: VoiceConsentAction,
        expected_consent_revision: u64,
        now_unix: u64,
    ) -> Result<VoiceConsentReceipt, rusqlite::Error> {
        validate_voice_provider(provider)?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let key: String = tx.query_row(
            "SELECT semantic_key FROM user_model_revisions WHERE id = ?1",
            [head],
            |row| row.get(0),
        )?;
        if action == VoiceConsentAction::Grant
            && !active_heads_from_connection(&tx, now_unix, Some(&key))?
                .iter()
                .any(|r| r.id == head)
        {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                HeadConflict,
            )));
        }
        let current = latest(&tx, head, provider)?.map_or(0, |r| r.revision);
        if current != expected_consent_revision {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                HeadConflict,
            )));
        }
        let receipt = VoiceConsentReceipt {
            id: uuid::Uuid::new_v4().to_string(),
            head_revision_id: head.into(),
            provider: provider.into(),
            revision: current + 1,
            action,
            reviewer: "owner_operator".into(),
            at_unix: now_unix,
        };
        tx.execute(
            "INSERT INTO user_model_voice_consent_receipts
             (id, head_revision_id, provider, revision, action, reviewer, at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                receipt.id,
                head,
                provider,
                receipt.revision,
                match action {
                    VoiceConsentAction::Grant => "grant",
                    VoiceConsentAction::Revoke => "revoke",
                },
                receipt.reviewer,
                now_unix
            ],
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Fresh snapshot on every export: current heads, provenance, Scope and
    /// the latest consent receipt must all agree in ONE read transaction.
    pub fn voice_export_heads(
        &self,
        context: &ApplicabilityContext,
        provider: &str,
        now_unix: u64,
    ) -> Result<Vec<UserModelRevision>, rusqlite::Error> {
        validate_voice_provider(provider)?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let heads = active_heads_from_connection(&tx, now_unix, None)?;
        let heads = admit_agent_heads(&tx, heads, &context.agent_alias)?;
        let mut exported = Vec::new();
        for head in heads {
            if context.applies_str(&head.scope)
                && latest(&tx, &head.id, provider)?
                    .is_some_and(|r| r.action == VoiceConsentAction::Grant)
            {
                exported.push(head);
            }
        }
        tx.commit()?;
        Ok(exported)
    }
}

#[cfg(test)]
#[path = "user_model_voice_tests.rs"]
mod tests;
