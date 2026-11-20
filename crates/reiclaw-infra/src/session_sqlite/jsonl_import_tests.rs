use super::*;
use crate::session_backend::SessionBackend;
use std::path::PathBuf;
use tempfile::TempDir;

const ORIGINAL: &[u8] = b"{\"role\":\"user\",\"content\":\"first retained\"}\n{\"role\":\"assistant\",\"content\":\"second retained\"}\n";

fn fixture() -> (TempDir, SqliteSessionBackend, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    let source = dir.path().join("sessions/legacy.jsonl");
    let archive = source.with_extension("jsonl.migrated");
    std::fs::write(&source, ORIGINAL).unwrap();
    (dir, store, source, archive)
}

fn counts(store: &SqliteSessionBackend) -> (i64, i64, i64, i64) {
    store
        .conn
        .lock()
        .query_row(
            "SELECT (SELECT count(*) FROM sessions),
                (SELECT COALESCE(sum(message_count), 0) FROM session_metadata),
                (SELECT count(*) FROM sessions_fts WHERE sessions_fts MATCH 'retained'),
                (SELECT count(*) FROM session_jsonl_imports)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

fn synchronous(store: &SqliteSessionBackend) -> i64 {
    store
        .conn
        .lock()
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .unwrap()
}

#[test]
fn failed_second_message_rolls_back_history_metadata_fts_and_receipt() {
    let (dir, store, source, archive) = fixture();
    let prior_sync = synchronous(&store);
    store
        .conn
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_second BEFORE INSERT ON sessions
         WHEN new.content = 'second retained'
         BEGIN SELECT RAISE(ABORT, 'fixture message failure'); END;",
        )
        .unwrap();
    assert!(store.migrate_from_jsonl(dir.path()).is_err());
    assert_eq!(counts(&store), (0, 0, 0, 0));
    assert_eq!(std::fs::read(&source).unwrap(), ORIGINAL);
    assert!(!archive.exists());
    assert_eq!(synchronous(&store), prior_sync);

    store
        .conn
        .lock()
        .execute_batch("DROP TRIGGER reject_second")
        .unwrap();
    assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 1);
    assert_eq!(counts(&store), (2, 2, 2, 1));
    assert_eq!(std::fs::read(&archive).unwrap(), ORIGINAL);
    assert!(!source.exists());
    assert_eq!(synchronous(&store), prior_sync);
}

#[test]
fn archive_failure_rolls_back_and_never_overwrites_existing_bytes() {
    for blocked_by_directory in [false, true] {
        let (dir, store, source, archive) = fixture();
        if blocked_by_directory {
            std::fs::create_dir(&archive).unwrap();
        } else {
            crate::owned_archive::write_verified_bytes(&archive, b"preserve older archive")
                .unwrap();
        }
        assert!(store.migrate_from_jsonl(dir.path()).is_err());
        assert_eq!(counts(&store), (0, 0, 0, 0));
        assert_eq!(std::fs::read(&source).unwrap(), ORIGINAL);
        if blocked_by_directory {
            assert!(archive.is_dir());
            std::fs::remove_dir(&archive).unwrap();
        } else {
            assert_eq!(std::fs::read(&archive).unwrap(), b"preserve older archive");
            std::fs::remove_file(&archive).unwrap();
        }
        assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 1);
        assert_eq!(counts(&store), (2, 2, 2, 1));
    }
}

#[test]
fn receipt_or_commit_failure_keeps_complete_source_and_allows_retry() {
    for fail_at_commit in [false, true] {
        let (dir, store, source, archive) = fixture();
        if fail_at_commit {
            store
                .conn
                .lock()
                .execute_batch(
                    "PRAGMA foreign_keys = ON;
                 CREATE TABLE import_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE import_commit_guard (
                     parent_id INTEGER REFERENCES import_parent(id) DEFERRABLE INITIALLY DEFERRED
                 );
                 CREATE TRIGGER reject_receipt AFTER INSERT ON session_jsonl_imports BEGIN
                     INSERT INTO import_commit_guard (parent_id) VALUES (1);
                 END;",
                )
                .unwrap();
        } else {
            store
                .conn
                .lock()
                .execute_batch(
                    "CREATE TRIGGER reject_receipt BEFORE INSERT ON session_jsonl_imports
                 BEGIN SELECT RAISE(ABORT, 'fixture receipt failure'); END;",
                )
                .unwrap();
        }
        assert!(store.migrate_from_jsonl(dir.path()).is_err());
        assert_eq!(counts(&store), (0, 0, 0, 0));
        assert_eq!(std::fs::read(&source).unwrap(), ORIGINAL);
        assert_eq!(std::fs::read(&archive).unwrap(), ORIGINAL);
        store
            .conn
            .lock()
            .execute_batch("DROP TRIGGER reject_receipt")
            .unwrap();
        assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 1);
        assert_eq!(counts(&store), (2, 2, 2, 1));
    }
}

#[test]
fn committed_source_leftover_is_finalized_after_reopen_without_replay() {
    let (dir, store, source, archive) = fixture();
    assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 1);
    // Reproduce the durable state after COMMIT but before the source unlink.
    std::fs::write(&source, ORIGINAL).unwrap();
    drop(store);
    let reopened = SqliteSessionBackend::new(dir.path()).unwrap();
    assert_eq!(reopened.migrate_from_jsonl(dir.path()).unwrap(), 0);
    assert_eq!(counts(&reopened), (2, 2, 2, 1));
    assert!(!source.exists());
    assert_eq!(std::fs::read(&archive).unwrap(), ORIGINAL);
}

#[test]
fn source_changed_after_receipt_is_preserved_for_reconciliation() {
    let (dir, store, source, archive) = fixture();
    assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 1);
    let mut changed = ORIGINAL.to_vec();
    changed.extend_from_slice(b"{\"role\":\"user\",\"content\":\"new retained\"}\n");
    std::fs::write(&source, &changed).unwrap();
    assert!(store.migrate_from_jsonl(dir.path()).is_err());
    assert_eq!(counts(&store), (2, 2, 2, 1));
    assert_eq!(std::fs::read(&source).unwrap(), changed);
    assert_eq!(std::fs::read(&archive).unwrap(), ORIGINAL);
}

#[test]
fn receipt_survives_session_deletion_and_prevents_history_resurrection() {
    let (dir, store, source, _) = fixture();
    assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 1);
    assert!(store.delete_session("legacy").unwrap());
    assert_eq!(counts(&store), (0, 0, 0, 1));
    std::fs::write(&source, ORIGINAL).unwrap();
    drop(store);
    let reopened = SqliteSessionBackend::new(dir.path()).unwrap();
    assert_eq!(reopened.migrate_from_jsonl(dir.path()).unwrap(), 0);
    assert_eq!(counts(&reopened), (0, 0, 0, 1));
    assert!(!source.exists());
}

#[test]
fn receiptless_existing_history_is_not_guessed_or_appended() {
    let (dir, store, source, archive) = fixture();
    store
        .append("legacy", &ChatMessage::user("already retained"))
        .unwrap();
    assert!(store.migrate_from_jsonl(dir.path()).is_err());
    assert_eq!(counts(&store), (1, 1, 1, 0));
    assert_eq!(std::fs::read(&source).unwrap(), ORIGINAL);
    assert!(!archive.exists());
}

#[test]
fn malformed_and_invalid_utf8_lines_are_preserved_without_lossy_import() {
    let (dir, store, source, archive) = fixture();
    let mut original =
        b"\nmalformed legacy line\n{\"role\":\"user\",\"content\":\"bad \xff byte\"}\n".to_vec();
    original.extend_from_slice(ORIGINAL);
    std::fs::write(&source, &original).unwrap();
    assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 1);
    assert_eq!(counts(&store), (2, 2, 2, 1));
    assert_eq!(std::fs::read(&archive).unwrap(), original);
    assert!(!source.exists());
}

#[test]
fn files_without_valid_messages_remain_unclaimed() {
    for has_existing_history in [false, true] {
        let (dir, store, source, archive) = fixture();
        if has_existing_history {
            store
                .append("legacy", &ChatMessage::user("already retained"))
                .unwrap();
        }
        let before = counts(&store);
        std::fs::write(&source, b"\nmalformed\n\xff\n").unwrap();
        assert_eq!(store.migrate_from_jsonl(dir.path()).unwrap(), 0);
        assert_eq!(counts(&store), before);
        assert_eq!(std::fs::read(&source).unwrap(), b"\nmalformed\n\xff\n");
        assert!(!archive.exists());
    }
}

#[test]
fn independent_openers_import_once_under_the_sqlite_write_lock() {
    let (dir, store, source, archive) = fixture();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let total: usize = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let barrier = barrier.clone();
                let path = dir.path();
                scope.spawn(move || {
                    let backend = SqliteSessionBackend::new(path).unwrap();
                    barrier.wait();
                    backend.migrate_from_jsonl(path).unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .sum()
    });
    assert_eq!(total, 1);
    assert_eq!(counts(&store), (2, 2, 2, 1));
    assert!(!source.exists());
    assert_eq!(std::fs::read(&archive).unwrap(), ORIGINAL);
}

#[cfg(unix)]
#[test]
fn symlinked_source_or_archive_cannot_redirect_the_import() {
    use std::os::unix::fs::symlink;
    for source_link in [false, true] {
        let (dir, store, source, archive) = fixture();
        let outside = dir.path().join("unrelated.jsonl");
        std::fs::write(&outside, ORIGINAL).unwrap();
        if source_link {
            std::fs::remove_file(&source).unwrap();
            symlink(&outside, &source).unwrap();
        } else {
            symlink(&outside, &archive).unwrap();
        }
        assert!(store.migrate_from_jsonl(dir.path()).is_err());
        assert_eq!(counts(&store), (0, 0, 0, 0));
        assert_eq!(std::fs::read(&outside).unwrap(), ORIGINAL);
        assert!(source.exists());
    }
}
