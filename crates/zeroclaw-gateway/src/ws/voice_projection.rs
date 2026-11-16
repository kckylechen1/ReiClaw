//! Closed ADR-023 handshake view; never serialize SoulProfile or a User Model row.
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use zeroclaw_config::schema::Config;
use zeroclaw_memory::companion::{
    ApplicabilityContext, SOUL_PROFILE_DB_FILE, SoulProfileStore,
    USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS, UserModelStore, project_active_heads,
    resolve_effective_tone, seed_name_for_agent, validate_voice_provider,
};
use zeroclaw_runtime::agent::persona_projection::{render_identity, render_principles};

#[derive(Debug, serde::Serialize)]
pub(super) struct VoiceProjection {
    version: u32,
    provider: String,
    identity: String,
    tone: String,
    principles: String,
    owner_facts: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    MissingProvider,
    InvalidProvider,
    UnknownAlias,
    DisabledAlias,
    DeviceCredentialRequired,
    SoulUnavailable,
    OwnerIdentityRequired,
    UserModelUnavailable,
    Unavailable,
}

impl Error {
    fn code(self) -> &'static str {
        match self {
            Self::MissingProvider => "VOICE_PROVIDER_REQUIRED",
            Self::InvalidProvider => "VOICE_PROVIDER_INVALID",
            Self::UnknownAlias => "VOICE_UNKNOWN_ALIAS",
            Self::DisabledAlias => "VOICE_ALIAS_DISABLED",
            Self::DeviceCredentialRequired => "VOICE_DEVICE_CREDENTIAL_REQUIRED",
            Self::SoulUnavailable => "VOICE_SOUL_UNAVAILABLE",
            Self::OwnerIdentityRequired => "VOICE_OWNER_IDENTITY_REQUIRED",
            Self::UserModelUnavailable => "VOICE_USER_MODEL_UNAVAILABLE",
            Self::Unavailable => "VOICE_PROJECTION_UNAVAILABLE",
        }
    }

    pub(super) fn frame(self) -> serde_json::Value {
        serde_json::json!({"type":"error", "code": self.code(),
            "message": zeroclaw_runtime::i18n::get_required_cli_string("gateway-voice-projection-unavailable")})
    }

    pub(super) fn response(self) -> Response {
        let status = match self {
            Self::DisabledAlias | Self::DeviceCredentialRequired => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_REQUEST,
        };
        (status, axum::Json(self.frame())).into_response()
    }
}

pub(super) fn validate_alias(config: &Config, alias: &str) -> Result<(), Error> {
    match config.agent(alias) {
        None => Err(Error::UnknownAlias),
        Some(agent) if !agent.enabled => Err(Error::DisabledAlias),
        Some(_) => Ok(()),
    }
}

pub(super) fn provider(value: Option<&str>) -> Result<&str, Error> {
    let provider = value
        .filter(|s| !s.is_empty())
        .ok_or(Error::MissingProvider)?;
    validate_voice_provider(provider).map_err(|_| Error::InvalidProvider)?;
    Ok(provider)
}

pub(super) fn build(
    config: &Config,
    alias: &str,
    session: &str,
    carrier: Option<&str>,
) -> Result<VoiceProjection, Error> {
    let provider = provider(carrier)?;
    validate_alias(config, alias)?;
    if !config
        .data_dir
        .join(SOUL_PROFILE_DB_FILE)
        .try_exists()
        .map_err(|_| Error::SoulUnavailable)?
    {
        return Err(Error::OwnerIdentityRequired);
    }
    let soul = SoulProfileStore::shared(&config.data_dir).map_err(|_| Error::SoulUnavailable)?;
    let profile = soul.profile(alias).map_err(|_| Error::SoulUnavailable)?;
    if !profile.identity_is_owner_authored() {
        return Err(Error::OwnerIdentityRequired);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::Unavailable)?
        .as_secs();
    // Match persona_projection's defaults for any absent Principles, after
    // requiring the owner Identity. Never fall back to its degraded rendering.
    let profile = soul
        .ensure_seeded(alias, seed_name_for_agent(alias), now)
        .map_err(|_| Error::SoulUnavailable)?;
    if !profile.identity_is_owner_authored() {
        return Err(Error::OwnerIdentityRequired);
    }
    let identity = render_identity(&profile).ok_or(Error::OwnerIdentityRequired)?;
    let principles = render_principles(&profile).unwrap_or_default();
    let tone = resolve_effective_tone(config, alias, Some(&profile))
        .0
        .to_prompt_section()
        .unwrap_or_default();
    let context = ApplicabilityContext::new(alias, super::WS_CHANNEL_KEY, session);
    let heads = UserModelStore::shared(&config.data_dir)
        .and_then(|store| store.voice_export_heads(&context, provider, now))
        .map_err(|_| Error::UserModelUnavailable)?;
    Ok(VoiceProjection {
        version: 1,
        provider: provider.into(),
        identity: identity.trim_end().to_owned(),
        tone: tone.trim_end().to_owned(),
        principles: principles.trim_end().to_owned(),
        owner_facts: project_active_heads(&heads, USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS)
            .prompt_section,
    })
}
