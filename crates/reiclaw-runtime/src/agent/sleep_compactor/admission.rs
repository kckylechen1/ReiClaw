//! Durable admission for automatic memory work. Session routing is not proof
//! that the input came from the owner: device bridges use the owner's alias.

use std::sync::Arc;

use reiclaw_api::companion::{
    AuthorityClass, CompanionIngress, IngressIdentity, classify_companion_authority,
};
use reiclaw_api::memory_traits::{EpisodeRecord, EpisodeSourceKind};
use reiclaw_api::review::UserMessageSource;
use reiclaw_config::schema::Config;
use reiclaw_infra::session_backend::{SessionBackend, TimestampedMessage};

/// The same admission is used by background transcript reads, Episode reads,
/// and automatic writes. No new persistent permission store is needed: trusted
/// ingress is already stored atomically with each transcript row.
#[derive(Clone)]
pub struct SleepMemoryAdmission {
    config: Arc<Config>,
    pub(super) backend: Arc<dyn SessionBackend>,
    agent_alias: String,
}

impl SleepMemoryAdmission {
    pub fn new(config: &Config, backend: Arc<dyn SessionBackend>, agent_alias: &str) -> Self {
        Self {
            config: Arc::new(config.clone()),
            backend,
            agent_alias: agent_alias.to_string(),
        }
    }

    fn source_is_owner(&self, source: &UserMessageSource) -> bool {
        match source {
            UserMessageSource::Operator => true,
            UserMessageSource::Channel { sender_id } => {
                let ingress =
                    CompanionIngress::from_channel_identity(IngressIdentity::new(sender_id));
                classify_companion_authority(&ingress, &self.config.companion_memory.owner.gate())
                    == AuthorityClass::OwnerAuthored
            }
        }
    }

    /// Reject mixed or unattributed transcript windows. Assistant/tool rows
    /// are subordinate evidence only after a proven owner input; they cannot
    /// establish authority by themselves. Legacy and device rows stay intact
    /// in session storage but are never silently promoted to owner memory.
    pub fn permits_messages<'a>(
        &self,
        messages: impl IntoIterator<Item = &'a TimestampedMessage>,
    ) -> bool {
        let mut owner_turn = false;
        for row in messages {
            match row.message.role.as_str() {
                "user" => {
                    let Some(ingress) = &row.ingress else {
                        return false;
                    };
                    if !self.source_is_owner(&ingress.source) {
                        return false;
                    }
                    owner_turn = true;
                }
                "assistant" | "tool" if owner_turn => {}
                _ => return false,
            }
        }
        owner_turn
    }

    /// Blocking storage read; async callers run this on Tokio's blocking pool.
    /// Current device namespace checks are defense in depth. Persisted ingress
    /// remains authoritative when a bridge is renamed or removed from config.
    pub fn permits_session(&self, session_key: &str) -> bool {
        if !self.config.memory.auto_save {
            return false;
        }
        let Some(meta) = self.backend.get_session_metadata(session_key) else {
            return false;
        };
        if match meta.agent_alias.as_deref() {
            Some(alias) => alias != self.agent_alias,
            None => self.config.agents.len() != 1,
        } {
            return false;
        }
        if let Some(raw_session) = session_key.strip_prefix("gw_") {
            let sanitized = reiclaw_api::session_keys::sanitize_session_key(raw_session);
            if self.config.gateway.bridges.values().any(|bridge| {
                bridge.device.is_some()
                    && (bridge.allows_session(raw_session) || bridge.allows_session(&sanitized))
            }) {
                return false;
            }
        }
        self.permits_messages(self.backend.load_with_timestamps(session_key).iter())
    }

    /// Delegation digests come from the verified terminal-result producer,
    /// not from unauthenticated chat. Dialogue records require their durable
    /// source proof, including records written before this boundary existed.
    pub fn permits_episode(&self, episode: &EpisodeRecord) -> bool {
        self.config.memory.auto_save
            && episode.agent_id == self.agent_alias
            && (episode.source_kind == EpisodeSourceKind::DelegationDigest
                || self.permits_session(&episode.session_key))
    }
}
