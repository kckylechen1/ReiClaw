//! Refresh the foreground view only at a settled turn boundary.

use super::{AppState, WsSession, WsTurnScope, device};
use std::sync::Arc;

pub(super) async fn refresh(
    state: &AppState,
    session: &mut WsSession,
    scope: &WsTurnScope,
) -> Option<reiclaw_api::agent::TurnEvent> {
    if !session.history_persisted
        || scope
            .bridge
            .as_ref()
            .is_some_and(device::Binding::device_bound)
    {
        return None;
    }
    let backend = state.session_backend.as_ref().map(Arc::clone)?;
    let key = scope.session_key.clone();
    let watermark = session.checkpoint_watermark;
    let loaded = tokio::task::spawn_blocking(move || {
        backend.load_session_window_if_advanced(&key, watermark)
    })
    .await;
    let window = match loaded {
        Ok(Ok(Some(window))) => window,
        Ok(Ok(None)) => return None,
        Ok(Err(error)) => {
            log_failure(&scope.session_key, &error.to_string());
            return None;
        }
        Err(error) => {
            log_failure(&scope.session_key, &error.to_string());
            return None;
        }
    };
    match session.agent.refresh_session_window_with_event(&window) {
        Ok(event) => {
            session.checkpoint_watermark = window
                .active_checkpoint
                .as_ref()
                .map_or(watermark, |checkpoint| checkpoint.up_to_message_id);
            event
        }
        Err(error) => {
            log_failure(&scope.session_key, &error.to_string());
            None
        }
    }
}

pub(super) fn log_failure(session_key: &str, error: &str) {
    ::reiclaw_log::record!(
        WARN,
        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
            .with_outcome(::reiclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "session_key": session_key,
                "error": error,
            })),
        "WS durable history operation failed; the live conversation was retained"
    );
}
