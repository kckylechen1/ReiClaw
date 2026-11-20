//! Daemon sleep worker: startup recovery, idle flush, and `04:00` Auto Dream.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use reiclaw_api::memory_traits::{EpisodeRecord, EpisodeSourceKind, Memory, MemoryCategory};
use reiclaw_api::tool::Tool;
use reiclaw_config::attention::AttentionConfig;
use reiclaw_config::multi_agent::MemoryBackendKind;
use reiclaw_config::schema::Config;
use reiclaw_infra::session_backend::{SessionBackend, SessionMetadata};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::tools::{
    ActionProposeTool, DigestPublishTool, EpisodeCommitTool, EpisodeReadTool, MemorySupersedeTool,
    SessionSearchTool, publish_dream_digests,
};
use super::{
    COLD_WINDOW_GAP_SECS, DEEP_DREAM_SYSTEM_PROMPT, DREAM_CUT_HOUR, DREAM_STATE_FILENAME,
    EPISODE_FLUSH_SYSTEM_PROMPT, ResolvedSleepModel, SLEEP_CHECK_INTERVAL, SharedOutcome,
    SleepMemoryAdmission, complete_batch_episode_id, complete_batch_episode_watermark,
    resolve_daemon_fast_sleep_model, resolve_primary_sleep_model, run_confined_loop,
    select_transcript_batch, truncate_utf8,
};
// ── Quiet Hours & 04:00 Day Cut Scheduling ──────────────────────────────────

/// Return `true` when `now` falls inside configured `AttentionConfig` quiet hours.
pub fn is_quiet_hours(policy: Option<&AttentionConfig>, now: jiff::Timestamp) -> bool {
    let Some(policy) = policy else {
        return false;
    };
    let Ok(timezone) = jiff::tz::TimeZone::get(&policy.timezone) else {
        return true;
    };
    let Ok(start) = jiff::civil::Time::strptime("%H:%M", &policy.quiet_start) else {
        return true;
    };
    let Ok(end) = jiff::civil::Time::strptime("%H:%M", &policy.quiet_end) else {
        return true;
    };
    let local = now.to_zoned(timezone).time();
    if start < end {
        local >= start && local < end
    } else {
        local >= start || local < end
    }
}

/// Resolved local `04:00` Auto Dream window for a given instant and timezone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamWindow {
    pub day_key: String,
    pub window_start: String,
    pub window_end: String,
}

/// Compute the most recently completed local `04:00` day-cut window for `now`.
pub fn due_dream_window(now: jiff::Timestamp, timezone: jiff::tz::TimeZone) -> Result<DreamWindow> {
    let zoned = now.to_zoned(timezone.clone());
    let cut_time = jiff::civil::time(DREAM_CUT_HOUR, 0, 0, 0);
    let end_date = if zoned.time() >= cut_time {
        zoned.date()
    } else {
        zoned.date().yesterday()?
    };
    let start_date = end_date.yesterday()?;
    let window_end_zoned = timezone.to_zoned(end_date.at(DREAM_CUT_HOUR, 0, 0, 0))?;
    let window_start_zoned = timezone.to_zoned(start_date.at(DREAM_CUT_HOUR, 0, 0, 0))?;
    Ok(DreamWindow {
        day_key: end_date.strftime("%Y-%m-%d").to_string(),
        window_start: reiclaw_api::time::format_rfc3339(window_start_zoned.timestamp()),
        window_end: reiclaw_api::time::format_rfc3339(window_end_zoned.timestamp()),
    })
}

fn resolve_dream_timezone(config: &Config) -> jiff::tz::TimeZone {
    if let Some(tz_name) = config
        .gateway
        .attention
        .as_ref()
        .map(|a| a.timezone.trim())
        .filter(|s| !s.is_empty())
        && let Ok(tz) = jiff::tz::TimeZone::get(tz_name)
    {
        return tz;
    }
    jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct DreamStateFile {
    #[serde(default)]
    last_day_key_by_agent: BTreeMap<String, String>,
}

fn dream_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DREAM_STATE_FILENAME)
}

fn load_dream_state(data_dir: &Path) -> DreamStateFile {
    let path = dream_state_path(data_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return DreamStateFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_dream_state(data_dir: &Path, state: &DreamStateFile) -> Result<()> {
    let path = dream_state_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = data_dir.join(format!(
        "{DREAM_STATE_FILENAME}.tmp.{}",
        uuid::Uuid::new_v4()
    ));
    let encoded = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp, encoded)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ── Daemon Worker: Startup Recovery, Idle Flush, Light Sleep & 04:00 Dream ──

fn agent_uses_memorycore(config: &Config, agent_alias: &str) -> bool {
    let Some(agent_cfg) = config.agent(agent_alias) else {
        return false;
    };
    if !agent_cfg.enabled {
        return false;
    }
    if matches!(
        agent_cfg.memory.backend,
        MemoryBackendKind::None | MemoryBackendKind::Markdown
    ) {
        return false;
    }
    true
}

fn session_belongs_to_agent(meta: &SessionMetadata, agent_alias: &str, single_agent: bool) -> bool {
    match meta.agent_alias.as_deref() {
        Some(owner) => owner == agent_alias,
        None => single_agent,
    }
}

/// Flush one complete, bounded batch into an `EpisodeRecord`. Later passes
/// resume after its exact watermark. Only promote a staged checkpoint when
/// Episode coverage has reached it. Used on startup and idle detection.
pub async fn flush_session_to_episode(
    config: &Config,
    backend: &Arc<dyn SessionBackend>,
    memory: &Arc<dyn Memory>,
    agent_alias: &str,
    session_key: &str,
    source_kind: EpisodeSourceKind,
    sleep_model: Option<&ResolvedSleepModel>,
) -> Result<bool> {
    let admission = SleepMemoryAdmission::new(config, Arc::clone(backend), agent_alias);
    let check = admission.clone();
    let key = session_key.to_string();
    if !tokio::task::spawn_blocking(move || check.permits_session(&key)).await? {
        return Ok(false);
    }
    let b = Arc::clone(backend);
    let k = session_key.to_string();
    let window = tokio::task::spawn_blocking(move || b.load_session_window(&k))
        .await
        .context("load_session_window failed")??;

    if window.delta_messages.is_empty() {
        return Ok(false);
    }
    if !admission.permits_messages(window.delta_messages.iter().map(|(_, row)| row)) {
        return Ok(false);
    }

    let existing = memory
        .read_episodes_in_window(agent_alias, None, None, Some(session_key), 100)
        .await?;
    let mut flushed_through = existing
        .iter()
        .filter_map(complete_batch_episode_watermark)
        .max()
        .unwrap_or(0);
    let mut first_unflushed = window
        .delta_messages
        .partition_point(|(id, _)| *id <= flushed_through);
    let batch = loop {
        let Some(batch) = select_transcript_batch(&window.delta_messages[first_unflushed..])?
        else {
            break None;
        };
        // Early recovery batches can sort behind hundreds of later legacy
        // records. An exact key+agent lookup proves coverage independently of
        // recency pagination, while namespace and record checks fail closed.
        let expected_id = complete_batch_episode_id(session_key, batch.up_to_message_id());
        let existing = memory.get_for_agent(&expected_id, agent_alias).await?;
        let covered = if let Some(entry) = existing.filter(|entry| {
            entry.key == expected_id
                && entry.namespace == "sleep_episode"
                && entry.category == MemoryCategory::Custom("sleep_episode".into())
                && entry.superseded_by.is_none()
                && entry.agent_id.as_deref() == Some(agent_alias)
                && entry.session_id.as_deref() == Some(session_key)
        }) {
            let episode: EpisodeRecord = serde_json::from_str(&entry.content)
                .context("invalid complete-batch Episode record")?;
            episode.validate()?;
            episode.agent_id == agent_alias
                && episode.session_key == session_key
                && episode.episode_id == expected_id
                && complete_batch_episode_watermark(&episode) == Some(batch.up_to_message_id())
        } else {
            false
        };
        if covered {
            flushed_through = batch.up_to_message_id();
            first_unflushed += batch.messages.len();
            continue;
        }
        break Some(batch);
    };
    let Some(batch) = batch else {
        if window
            .staged_checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.up_to_message_id <= flushed_through)
        {
            let b = Arc::clone(backend);
            let k = session_key.to_string();
            tokio::task::spawn_blocking(move || {
                b.promote_staged_checkpoint_if_covered(&k, flushed_through)
            })
            .await
            .context("episode-covered checkpoint promotion task failed")??;
        }
        return Ok(false);
    };
    let covered_message_id = batch.up_to_message_id();
    let (started_rfc, ended_rfc) = batch.timestamps();
    let outcome = SharedOutcome::new();
    let security = super::sleep_security_for_agent(config, agent_alias);

    if let Some(model) = sleep_model {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(SessionSearchTool {
                session_backend: Arc::clone(backend),
                allowed_session_keys: vec![session_key.to_string()],
                admission: admission.clone(),
            }),
            Box::new(EpisodeReadTool {
                memory: Arc::clone(memory),
                agent_id: agent_alias.to_string(),
                admission: admission.clone(),
            }),
            Box::new(EpisodeCommitTool {
                memory: Arc::clone(memory),
                memory_config: config.memory.clone(),
                agent_id: agent_alias.to_string(),
                default_session_key: session_key.to_string(),
                source_kind,
                default_started_at: started_rfc.clone(),
                default_ended_at: ended_rfc.clone(),
                default_up_to_message_id: Some(covered_message_id),
                tolerate_unsupported_sleep: false,
                outcome: outcome.clone(),
            }),
            Box::new(MemorySupersedeTool {
                memory: Arc::clone(memory),
                agent_id: agent_alias.to_string(),
                outcome: outcome.clone(),
            }),
        ];
        let prompt = format!(
            "Flush this complete batch from session `{session_key}` ({source_kind}) through message_id={covered_message_id}. Later raw rows, if any, are outside this batch.\n\nTranscript:\n{}",
            batch.transcript
        );
        run_confined_loop(
            model.provider.as_ref(),
            &model.provider_name,
            &model.model,
            model.temperature,
            agent_alias,
            tools,
            EPISODE_FLUSH_SYSTEM_PROMPT,
            &prompt,
            5,
            Some(&security),
        )
        .await?;
    }
    if outcome.0.lock().committed_episodes.is_empty() {
        // Retain every selected row even if there is no provider or the model
        // finishes without committing an Episode. Never truncate and advance.
        security
            .enforce_estop_tool("episode_commit")
            .map_err(anyhow::Error::msg)?;
        let episode = batch.fallback_episode(agent_alias, session_key, source_kind);
        episode.validate()?;
        memory.commit_episode(&episode).await?;
    }

    if window
        .staged_checkpoint
        .as_ref()
        .is_some_and(|checkpoint| checkpoint.up_to_message_id <= covered_message_id)
    {
        let b = Arc::clone(backend);
        let k = session_key.to_string();
        tokio::task::spawn_blocking(move || {
            b.promote_staged_checkpoint_if_covered(&k, covered_message_id)
        })
        .await
        .context("episode-covered checkpoint promotion task failed")??;
    }

    Ok(true)
}

/// Run one `04:00` Deep Auto Dream pass for `agent_alias` over `[window.window_start, window.window_end]`.
///
/// Uses the agent's primary flagship model (`primary_model`), respects existing
/// `owner_edited = true` `DailyDigest`s, updates `PinnedDigest` (`<= 8_000` bytes),
/// and respects quiet hours for any optional summary notification.
pub async fn run_deep_dream_for_agent(
    config: &Config,
    backend: &Arc<dyn SessionBackend>,
    memory: &Arc<dyn Memory>,
    agent_alias: &str,
    window: &DreamWindow,
    primary_model: &ResolvedSleepModel,
    now: jiff::Timestamp,
) -> Result<bool> {
    let admission = SleepMemoryAdmission::new(config, Arc::clone(backend), agent_alias);
    let episodes = memory
        .read_episodes_in_window(
            agent_alias,
            Some(&window.window_start),
            Some(&window.window_end),
            None,
            100,
        )
        .await?;
    let check = admission.clone();
    let episodes = tokio::task::spawn_blocking(move || {
        episodes
            .into_iter()
            .filter(|episode| check.permits_episode(episode))
            .collect::<Vec<_>>()
    })
    .await
    .context("dream episode admission task failed")?;

    if episodes.is_empty() {
        return Ok(false);
    }

    let allowed_session_keys: Vec<String> = {
        let b = Arc::clone(backend);
        let check = admission.clone();
        tokio::task::spawn_blocking(move || {
            b.list_sessions_with_metadata()
                .into_iter()
                .filter(|m| check.permits_session(&m.key))
                .map(|m| m.key)
                .collect()
        })
        .await
        .context("list_sessions_with_metadata failed")?
    };

    let existing_pinned = memory.load_pinned_digest(agent_alias).await?;
    let outcome = SharedOutcome::new();

    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(SessionSearchTool {
            session_backend: Arc::clone(backend),
            allowed_session_keys,
            admission: admission.clone(),
        }),
        Box::new(EpisodeReadTool {
            memory: Arc::clone(memory),
            agent_id: agent_alias.to_string(),
            admission,
        }),
        Box::new(MemorySupersedeTool {
            memory: Arc::clone(memory),
            agent_id: agent_alias.to_string(),
            outcome: outcome.clone(),
        }),
        Box::new(ActionProposeTool {
            outcome: outcome.clone(),
        }),
        Box::new(DigestPublishTool {
            memory: Arc::clone(memory),
            agent_id: agent_alias.to_string(),
            day_key: window.day_key.clone(),
            window_start: window.window_start.clone(),
            window_end: window.window_end.clone(),
            outcome: outcome.clone(),
        }),
    ];

    let episodes_json = serde_json::to_string_pretty(&episodes)?;
    let pinned_note = existing_pinned
        .as_ref()
        .map(|p| p.rendered_index.as_str())
        .unwrap_or("(none)");
    let prompt = format!(
        "Deep Auto Dream for agent `{agent_alias}`, day_key `{}` (window {} .. {}).\n\nExisting PinnedDigest:\n{}\n\nEpisodes in window:\n{}",
        window.day_key,
        window.window_start,
        window.window_end,
        truncate_utf8(pinned_note, 4_000),
        truncate_utf8(&episodes_json, 24_000),
    );

    let security = super::sleep_security_for_agent(config, agent_alias);
    let final_text = run_confined_loop(
        primary_model.provider.as_ref(),
        &primary_model.provider_name,
        &primary_model.model,
        primary_model.temperature,
        agent_alias,
        tools,
        DEEP_DREAM_SYSTEM_PROMPT,
        &prompt,
        8,
        Some(&security),
    )
    .await?;

    if outcome.0.lock().published_digest.is_none() {
        publish_dream_fallback(
            memory.as_ref(),
            agent_alias,
            window,
            &episodes,
            &final_text,
            &outcome,
        )
        .await?;
    }

    let quiet = is_quiet_hours(config.gateway.attention.as_ref(), now);
    ::reiclaw_log::record!(
        INFO,
        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Complete)
            .with_category(::reiclaw_log::EventCategory::Agent)
            .with_attrs(::serde_json::json!({
                "agent": agent_alias,
                "day_key": window.day_key,
                "episodes": episodes.len(),
                "quiet_hours": quiet,
                "preserved_owner_edits": outcome.0.lock().preserved_owner_digest,
            })),
        if quiet {
            "Auto Dream complete (summary notification suppressed during quiet hours)"
        } else {
            "Auto Dream complete"
        }
    );

    Ok(true)
}

#[derive(Debug, Deserialize)]
struct DreamFallbackJson {
    #[serde(default)]
    narrative: String,
    #[serde(default)]
    settled_decisions: Vec<String>,
    #[serde(default)]
    why_and_tradeoffs: Vec<String>,
    #[serde(default)]
    latent_insights: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    action_candidates: Vec<String>,
    #[serde(default)]
    pinned_index: String,
}

async fn publish_dream_fallback(
    memory: &dyn Memory,
    agent_id: &str,
    window: &DreamWindow,
    episodes: &[EpisodeRecord],
    final_text: &str,
    outcome: &SharedOutcome,
) -> Result<()> {
    let cleaned = final_text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(parsed) = serde_json::from_str::<DreamFallbackJson>(cleaned)
        && !parsed.narrative.trim().is_empty()
    {
        let _ = publish_dream_digests(
            memory,
            agent_id,
            &window.day_key,
            &window.window_start,
            &window.window_end,
            parsed.narrative.trim(),
            parsed.settled_decisions,
            parsed.why_and_tradeoffs,
            parsed.latent_insights,
            parsed.entities,
            parsed.action_candidates,
            &parsed.pinned_index,
            outcome,
        )
        .await?;
        return Ok(());
    }

    let mut settled = Vec::new();
    let mut why = Vec::new();
    let mut entities = Vec::new();
    for ep in episodes {
        for d in &ep.settled_decisions {
            if !settled.iter().any(|x| x == d) {
                settled.push(d.clone());
            }
        }
        for w in &ep.why_and_tradeoffs {
            if !why.iter().any(|x| x == w) {
                why.push(w.clone());
            }
        }
        for e in &ep.entities {
            if !entities.iter().any(|x| x == e) {
                entities.push(e.clone());
            }
        }
    }
    let narrative = if !final_text.trim().is_empty() {
        truncate_utf8(final_text.trim(), 4_000)
    } else {
        episodes
            .iter()
            .map(|e| e.summary.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let _ = publish_dream_digests(
        memory,
        agent_id,
        &window.day_key,
        &window.window_start,
        &window.window_end,
        &narrative,
        settled,
        why,
        Vec::new(),
        entities,
        Vec::new(),
        "",
        outcome,
    )
    .await?;
    Ok(())
}

pub(super) async fn tick_agent(
    config: &Config,
    backend: &Arc<dyn SessionBackend>,
    dream_state: &mut DreamStateFile,
    agent_alias: &str,
    startup_pass: bool,
    now: jiff::Timestamp,
) -> Result<bool> {
    if !config.memory.auto_save || !agent_uses_memorycore(config, agent_alias) {
        return Ok(false);
    }

    let api_key = config
        .resolved_model_provider_for_agent(agent_alias)
        .and_then(|(_, _, cfg)| cfg.api_key.clone());
    let memory =
        reiclaw_memory::create_memory_for_agent(config, agent_alias, api_key.as_deref()).await?;

    let b = Arc::clone(backend);
    let sessions = tokio::task::spawn_blocking(move || b.list_sessions_with_metadata())
        .await
        .context("list_sessions_with_metadata failed")?;

    let single_agent = config.agents.len() == 1;
    let fast_model = resolve_daemon_fast_sleep_model(config, agent_alias);

    for meta in sessions {
        if !session_belongs_to_agent(&meta, agent_alias, single_agent) {
            continue;
        }
        let b_state = Arc::clone(backend);
        let key_owned = meta.key.clone();
        let state = tokio::task::spawn_blocking(move || b_state.get_session_state(&key_owned))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten();
        if state.as_ref().is_some_and(|s| s.state == "running") {
            continue;
        }

        let idle_secs = now
            .as_second()
            .saturating_sub(meta.last_activity.as_second());
        let (should_flush, source_kind) = if startup_pass {
            (true, EpisodeSourceKind::StartupRecovery)
        } else if idle_secs >= COLD_WINDOW_GAP_SECS {
            (true, EpisodeSourceKind::IdleFlush)
        } else {
            (false, EpisodeSourceKind::IdleFlush)
        };

        if should_flush
            && let Err(err) = flush_session_to_episode(
                config,
                backend,
                &memory,
                agent_alias,
                &meta.key,
                source_kind,
                fast_model.as_ref(),
            )
            .await
        {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "agent": agent_alias,
                        "session_key": meta.key,
                        "error": err.to_string(),
                    })),
                "sleep episode flush failed"
            );
        }
    }

    // The memory factory above owns configured light-sleep governance and its
    // hygiene cadence. A Dream tick must not run it again outside that window.

    let tz = resolve_dream_timezone(config);
    let window = due_dream_window(now, tz)?;
    let last_done = dream_state.last_day_key_by_agent.get(agent_alias);
    let dream_due = last_done.is_none_or(|prev| prev.as_str() < window.day_key.as_str());
    let mut state_dirty = false;
    if dream_due && let Some(primary_model) = resolve_primary_sleep_model(config, agent_alias) {
        run_deep_dream_for_agent(
            config,
            backend,
            &memory,
            agent_alias,
            &window,
            &primary_model,
            now,
        )
        .await?;
        dream_state
            .last_day_key_by_agent
            .insert(agent_alias.to_string(), window.day_key);
        state_dirty = true;
    }

    Ok(state_dirty)
}

/// Daemon worker: handles startup session recovery, periodic idle session
/// flushes, light-sleep governance, and the local `04:00` Auto Dream (including
/// missed-window catch-up via a lightweight JSON marker in `config.data_dir`).
pub async fn run_worker(config: Config, cancel: CancellationToken) -> Result<()> {
    let mut interval = tokio::time::interval(SLEEP_CHECK_INTERVAL);
    let mut startup_pass = true;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if config.agents.is_empty() || !config.data_dir.is_dir() {
            startup_pass = false;
            continue;
        }
        let backend = reiclaw_infra::make_session_backend(&config.data_dir)?;
        let mut dream_state = load_dream_state(&config.data_dir);
        let mut state_dirty = false;

        let mut aliases: Vec<String> = config.agents.keys().cloned().collect();
        aliases.sort();
        let now = jiff::Timestamp::now();
        for alias in &aliases {
            match tick_agent(
                &config,
                &backend,
                &mut dream_state,
                alias,
                startup_pass,
                now,
            )
            .await
            {
                Ok(dirty) => state_dirty |= dirty,
                Err(err) => {
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                            .with_outcome(::reiclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "agent": alias,
                                "error": err.to_string(),
                            })),
                        "sleep compactor worker tick failed"
                    );
                }
            }
        }
        if state_dirty && let Err(err) = save_dream_state(&config.data_dir, &dream_state) {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({ "error": err.to_string() })),
                "failed to persist sleep_dream_state.json"
            );
        }
        startup_pass = false;
    }
}
