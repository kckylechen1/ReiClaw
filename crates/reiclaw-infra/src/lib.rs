//! Channel and session infrastructure: session backends, outbox, and process/network guards.

pub mod acp_session_store;
pub mod bridge_outbox;
pub mod gateway_session_owner;
pub mod net_guard;
pub mod owned_archive;
pub mod process;
pub mod session_backend;
pub mod session_queue;
pub mod session_sqlite;
pub mod sqlite_perms;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use crate::session_backend::SessionBackend;

pub fn effective_gateway_bind_socket_addr(host: &str, port: u16) -> SocketAddr {
    parse_gateway_bind_socket_addr(host, port)
        .unwrap_or_else(|_| fallback_gateway_bind_socket_addr(port))
}

pub fn parse_gateway_bind_socket_addr(
    host: &str,
    port: u16,
) -> Result<SocketAddr, std::net::AddrParseError> {
    format!("{host}:{port}").parse()
}

pub fn fallback_gateway_bind_socket_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Open the single live session backend, including the retained legacy import.
pub fn make_session_backend(workspace_dir: &Path) -> std::io::Result<Arc<dyn SessionBackend>> {
    Ok(Arc::new(open_sqlite_with_jsonl_import(workspace_dir)?))
}

fn open_sqlite_with_jsonl_import(
    workspace_dir: &Path,
) -> std::io::Result<session_sqlite::SqliteSessionBackend> {
    let backend = session_sqlite::SqliteSessionBackend::new(workspace_dir)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    match backend.migrate_from_jsonl(workspace_dir) {
        Ok(0) => {}
        Ok(n) => ::reiclaw_log::record!(
            INFO,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note),
            &format!(
                "session_backend=sqlite: imported {n} legacy JSONL session(s) from \
             {}/sessions; renamed to *.jsonl.migrated.",
                workspace_dir.display()
            )
        ),
        Err(e) => ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"e": e.to_string()})),
            "session_backend=sqlite: JSONL import skipped: . Existing JSONL \
             sessions remain on disk; review the import error and repair the source \
             before retrying. The JSONL runtime backend is retired."
        ),
    }
    Ok(backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reiclaw_api::model_provider::ChatMessage;
    use tempfile::TempDir;

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage::user(content)
    }

    #[test]
    fn make_session_backend_sqlite_round_trips_through_sqlite_db() {
        let tmp = TempDir::new().unwrap();
        let backend = make_session_backend(tmp.path()).unwrap();
        backend.append("k1", &user_msg("hello-sqlite")).unwrap();
        let loaded = backend.load("k1");
        assert_eq!(loaded.len(), 1);
        let db = tmp.path().join("sessions").join("sessions.db");
        assert!(db.exists(), "sqlite db must be written under sessions/");
        // The JSONL companion file must NOT have been created.
        assert!(!tmp.path().join("sessions").join("k1.jsonl").exists());
    }

    #[test]
    fn make_session_backend_sqlite_imports_legacy_jsonl_on_first_open() {
        // Seed JSONL session files, then open SQLite — the .jsonl files must
        // be migrated and the imported sessions must be visible via the new
        // backend. The .jsonl files get renamed to .jsonl.migrated so the
        // operator can roll back.
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let original = "{\"role\":\"user\",\"content\":\"from-jsonl\"}\n";
        std::fs::write(sessions.join("legacy.jsonl"), original).unwrap();
        let sqlite = make_session_backend(tmp.path()).unwrap();
        let loaded = sqlite.load("legacy");
        assert_eq!(
            loaded.len(),
            1,
            "legacy JSONL session must hydrate via SQLite"
        );
        // .jsonl renamed to .jsonl.migrated; original gone.
        let jsonl_orig = tmp.path().join("sessions").join("legacy.jsonl");
        let jsonl_migrated = tmp.path().join("sessions").join("legacy.jsonl.migrated");
        assert!(!jsonl_orig.exists(), "original .jsonl should be renamed");
        assert!(
            jsonl_migrated.exists(),
            ".jsonl.migrated rollback file should remain"
        );
        assert_eq!(std::fs::read_to_string(&jsonl_migrated).unwrap(), original);
        drop(sqlite);
        let reopened = make_session_backend(tmp.path()).unwrap();
        assert_eq!(reopened.load("legacy").len(), 1, "no repeated import");
        assert_eq!(reopened.load("legacy")[0].content, "from-jsonl");
    }
}
