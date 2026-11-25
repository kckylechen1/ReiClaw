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

/// Open the single live SQLite session backend.
pub fn make_session_backend(workspace_dir: &Path) -> std::io::Result<Arc<dyn SessionBackend>> {
    let backend = session_sqlite::SqliteSessionBackend::new(workspace_dir)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(Arc::new(backend))
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
}
