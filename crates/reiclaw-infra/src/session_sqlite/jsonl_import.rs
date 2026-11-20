//! Retained JSONL compatibility import owned by `SqliteSessionBackend`.
//!
//! The receipt belongs to the existing sessions database, not session history:
//! clearing a session must not make a leftover source replay deleted messages.
//! Legacy writers must already be stopped; the selectable JSONL backend is
//! retired. Snapshot checks reject observed external changes, not arbitrary
//! non-cooperating filesystem writes between a check and an unlink.

use super::SqliteSessionBackend;
use anyhow::{Context, Result, ensure};
use reiclaw_api::model_provider::ChatMessage;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

pub(super) fn initialize(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_jsonl_imports (
            source_file TEXT PRIMARY KEY,
            source_sha256 TEXT NOT NULL CHECK(length(source_sha256) = 64),
            imported_messages INTEGER NOT NULL CHECK(imported_messages > 0)
         );",
    )
}

fn read_regular_source(path: &Path) -> std::io::Result<Vec<u8>> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy JSONL source or archive is not a regular file",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy JSONL source or archive changed to a non-regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn import_file(
    conn: &mut Connection,
    path: &Path,
    source_file: &str,
    session_key: &str,
) -> Result<bool> {
    // Independent factory handles use different connection mutexes. The SQLite
    // write lock, including the receipt read, is their common serialization.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT source_sha256 FROM session_jsonl_imports WHERE source_file = ?1",
            [source_file],
            |row| row.get(0),
        )
        .optional()?;
    let archive_path = path.with_extension("jsonl.migrated");
    let bytes = match read_regular_source(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && previous.is_some() => {
            // Another opener may have finished after our directory enumeration.
            let archived = read_regular_source(&archive_path)
                .context("verify completed legacy JSONL archive")?;
            ensure!(
                previous.as_deref() == Some(digest(&archived).as_str()),
                "completed legacy JSONL archive does not match its import receipt"
            );
            return Ok(false);
        }
        Err(error) => return Err(error).context("read complete legacy JSONL source"),
    };
    let source_digest = digest(&bytes);
    if let Some(previous) = &previous {
        ensure!(
            previous == &source_digest,
            "legacy JSONL source changed after import; preserve it for explicit reconciliation"
        );
    }

    let mut imported_messages = 0_i64;
    if previous.is_none() {
        // Keep the historical line-level tolerance without lossy UTF-8 repair.
        // Every original byte, including malformed lines, goes to the archive.
        let mut messages = bytes
            .split(|byte| *byte == b'\n')
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter_map(|line| serde_json::from_str::<ChatMessage>(line.trim()).ok())
            .peekable();
        if messages.peek().is_none() {
            return Ok(false);
        }
        let occupied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_key = ?1)
                 OR EXISTS(SELECT 1 FROM session_metadata WHERE session_key = ?1)",
            [session_key],
            |row| row.get(0),
        )?;
        ensure!(
            !occupied,
            "legacy JSONL session already exists without an import receipt; explicit reconciliation is required"
        );
        let now = reiclaw_api::time::format_rfc3339(jiff::Timestamp::now());
        for message in messages {
            SqliteSessionBackend::insert_message(&tx, session_key, &message, None, &now)
                .context("insert legacy JSONL message and metadata")?;
            imported_messages = imported_messages
                .checked_add(1)
                .context("legacy JSONL message count overflow")?;
        }
    }

    crate::owned_archive::write_verified_bytes(&archive_path, &bytes)
        .context("preserve complete legacy JSONL archive before commit")?;
    match read_regular_source(path) {
        Ok(current) => ensure!(
            current == bytes,
            "legacy JSONL source changed during import"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && previous.is_some() => {}
        Err(error) => return Err(error).context("recheck legacy JSONL source before commit"),
    }
    if previous.is_none() {
        tx.execute(
            "INSERT INTO session_jsonl_imports (source_file, source_sha256, imported_messages)
             VALUES (?1, ?2, ?3)",
            params![source_file, source_digest, imported_messages],
        )
        .context("record completed legacy JSONL import")?;
    }
    tx.commit().context("commit complete legacy JSONL import")?;

    // Commit and receipt precede removal. A crash or cleanup error leaves the
    // same source safe to retry, including after a user clears the session.
    match read_regular_source(path) {
        Ok(current) => {
            ensure!(
                current == bytes,
                "legacy JSONL source changed before cleanup"
            );
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error).context("remove committed legacy JSONL source");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context("verify committed legacy JSONL source for cleanup");
        }
    }
    Ok(previous.is_none())
}

impl SqliteSessionBackend {
    /// Import each legacy file atomically and archive its exact original bytes.
    /// Receipts prevent replays after cleanup failure or concurrent opens. This
    /// does not infer or repair imports performed by older, receipt-less code.
    pub fn migrate_from_jsonl(&self, workspace_dir: &Path) -> Result<usize> {
        let entries = match std::fs::read_dir(workspace_dir.join("sessions")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).context("read legacy JSONL sessions directory"),
        };
        let mut migrated = 0;
        for entry in entries {
            let entry = entry.context("enumerate legacy JSONL sessions")?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Some(session_key) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let mut conn = self.conn.lock();
            let prior_sync: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
            // The normal connection policy is restored on success and error.
            // FULL makes the commit durable before the only scanned source is
            // removed; the verified archive has already been synced too.
            conn.pragma_update(None, "synchronous", "FULL")?;
            let result = import_file(&mut conn, &entry.path(), &name, session_key);
            let restored = conn.pragma_update(None, "synchronous", prior_sync);
            let imported = match result {
                Ok(imported) => {
                    restored.context("restore session DB synchronous policy after JSONL import")?;
                    imported
                }
                Err(error) => {
                    if let Err(restore_error) = restored {
                        return Err(error).context(format!(
                            "also failed to restore session DB synchronous policy: {restore_error}"
                        ));
                    }
                    return Err(error);
                }
            };
            if imported {
                migrated += 1;
            }
        }
        Ok(migrated)
    }
}

#[cfg(test)]
#[path = "jsonl_import_tests.rs"]
mod tests;
