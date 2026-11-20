//! Foreground projection of the canonical active checkpoint and raw tail.

use super::{Agent, ConversationMessage, HistoryTrimNotice, TurnEvent};
use crate::agent::history_trim::{
    history_from_session_window, is_session_checkpoint_message, session_checkpoint_message,
};
use reiclaw_infra::session_backend::SessionWindow;

impl Agent {
    /// Restore a cold session from its active checkpoint and post-watermark
    /// messages. Staged checkpoints never enter the foreground prompt.
    pub fn seed_session_window_with_event(&mut self, window: &SessionWindow) -> Option<TurnEvent> {
        if self.history.is_empty()
            && let Ok(system) = self.build_system_prompt()
        {
            self.history.push(ConversationMessage::Chat(
                reiclaw_providers::ChatMessage::system(system),
            ));
        }
        self.history_has_trim_breadcrumb = window
            .active_checkpoint
            .as_ref()
            .is_some_and(|checkpoint| !checkpoint.digest.trim().is_empty());
        self.history.extend(
            history_from_session_window(window)
                .into_iter()
                .filter(|message| message.role != "system")
                .map(ConversationMessage::Chat),
        );
        self.trim_history(None)
            .map(HistoryTrimNotice::into_turn_event)
    }

    /// Adopt a newly promoted checkpoint between turns, retaining native tool
    /// calls/results in the uncheckpointed tail. Callers must first ensure that
    /// every completed foreground turn was persisted successfully.
    pub fn refresh_session_window_with_event(
        &mut self,
        window: &SessionWindow,
    ) -> anyhow::Result<Option<TurnEvent>> {
        let history = project_live_window(&self.history, self.history_has_trim_breadcrumb, window)?;
        self.history = history;
        self.history_has_trim_breadcrumb = true;
        Ok(self
            .trim_history(None)
            .map(HistoryTrimNotice::into_turn_event))
    }

    pub(super) fn leading_session_checkpoint(&self) -> Option<&ConversationMessage> {
        if !self.history_has_trim_breadcrumb {
            return None;
        }
        self.history
            .iter()
            .find(|message| !is_system(message))
            .filter(|message| {
                matches!(message, ConversationMessage::Chat(chat) if is_session_checkpoint_message(chat))
            })
    }
}

fn is_system(message: &ConversationMessage) -> bool {
    matches!(message, ConversationMessage::Chat(chat) if chat.role == "system")
}

fn project_live_window(
    history: &[ConversationMessage],
    has_synthetic_prefix: bool,
    window: &SessionWindow,
) -> anyhow::Result<Vec<ConversationMessage>> {
    let checkpoint = window
        .active_checkpoint
        .as_ref()
        .filter(|checkpoint| !checkpoint.digest.trim().is_empty())
        .ok_or_else(|| {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                    .with_category(::reiclaw_log::EventCategory::Agent)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure),
                "Foreground refresh requires an active checkpoint"
            );
            anyhow::Error::msg("foreground refresh requires an active checkpoint")
        })?;
    let prefix = history.iter().position(|message| !is_system(message));
    let chats: Vec<_> = history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            ConversationMessage::Chat(chat)
                if chat.role != "system" && !(has_synthetic_prefix && Some(index) == prefix) =>
            {
                Some((index, chat))
            }
            _ => None,
        })
        .collect();
    let delta: Vec<_> = window
        .delta_messages
        .iter()
        .map(|(_, message)| &message.message)
        .filter(|message| message.role != "system")
        .collect();
    let matched = chats.len().min(delta.len());
    for ((_, live), stored) in chats[chats.len() - matched..]
        .iter()
        .zip(&delta[delta.len() - matched..])
    {
        anyhow::ensure!(
            live.role == stored.role && live.content == stored.content,
            "foreground history differs from the persisted checkpoint tail"
        );
    }

    let mut projected: Vec<_> = history
        .iter()
        .take_while(|message| is_system(message))
        .cloned()
        .collect();
    projected.push(ConversationMessage::Chat(session_checkpoint_message(
        checkpoint,
    )));
    projected.extend(
        delta[..delta.len() - matched]
            .iter()
            .map(|message| ConversationMessage::Chat((*message).clone())),
    );
    if matched > 0 {
        let mut tail_start = chats[chats.len() - matched].0;
        // A restored/trimmed tail can begin at an assistant chat after a
        // native tool exchange; keep that adjacent exchange intact.
        while tail_start > 0 && !matches!(history[tail_start - 1], ConversationMessage::Chat(_)) {
            tail_start -= 1;
        }
        projected.extend_from_slice(&history[tail_start..]);
    }
    Ok(projected)
}

#[cfg(test)]
mod tests;
