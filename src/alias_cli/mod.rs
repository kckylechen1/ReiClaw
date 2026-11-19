//! CLI for alias CRUD: `rei {agents,providers,channels}
//! {create,list,rename,delete}`.

use anyhow::{Context, Result, bail};
use reiclaw::{AgentsCommands, ChannelsCommands, ProvidersCommands};
use reiclaw_config::alias_refs::{
    self, AliasKind, CascadeError, CascadePolicy, ProviderCategory, RenameError,
};
use reiclaw_config::schema::Config;

fn parse_provider_category(category: &str) -> Result<ProviderCategory> {
    match category {
        "models" => Ok(ProviderCategory::Models),
        "tts" => Ok(ProviderCategory::Tts),
        "transcription" => Ok(ProviderCategory::Transcription),
        other => {
            bail!("unknown provider category `{other}` (expected models | tts | transcription)")
        }
    }
}

/// The map-key section path for a kind (e.g. `agents`, `providers.models.anthropic`,
/// `channels.telegram`).
fn section_path(kind: &AliasKind) -> String {
    match kind {
        AliasKind::Agent => "agents".to_string(),
        AliasKind::Provider { category, family } => {
            let cat = match category {
                ProviderCategory::Models => "models",
                ProviderCategory::Tts => "tts",
                ProviderCategory::Transcription => "transcription",
            };
            format!("providers.{cat}.{family}")
        }
        AliasKind::Channel { channel_type } => format!("channels.{channel_type}"),
    }
}

fn list_section(config: &Config, section: &str) -> Result<()> {
    match config.get_map_keys(section) {
        Some(mut keys) => {
            keys.sort();
            if keys.is_empty() {
                println!("(no entries under {section})");
            } else {
                for k in keys {
                    println!("{k}");
                }
            }
        }
        None => bail!("no such config section: {section}"),
    }
    Ok(())
}

fn create_entry(config: &mut Config, section: &str, alias: &str) -> Result<bool> {
    // Shared guarded boundary: refuses the reserved `default` agent here too (an
    // operator create surface), and delegates unchanged for every other section.
    // The Reserved rejection is localized via Fluent like the delete/rename guards
    // below; Invalid (unknown section) keeps its pre-existing bare error.
    let created = match alias_refs::create_map_key_checked(config, section, alias) {
        Ok(created) => created,
        Err(alias_refs::CreateError::Reserved(_)) => {
            bail!("the `default` agent is reserved and cannot be created")
        }
        Err(alias_refs::CreateError::Invalid(msg)) => return Err(anyhow::Error::msg(msg)),
    };
    if created {
        config.mark_dirty(&format!("{section}.{alias}"));
        println!("created {section}.{alias}");
    } else {
        println!("{section}.{alias} already exists (no change)");
    }
    Ok(created)
}

/// Print the dry-run impact (blockers + scrubs) for a delete.
fn print_impact(kind: &AliasKind, alias: &str, config: &Config) {
    let report = alias_refs::plan_delete(config, kind, alias);
    let section = section_path(kind);
    if report.blockers.is_empty() {
        let count = report.scrubs.len().to_string();
        println!(
            "deleting {section}.{alias} would scrub {count} reference(s):",
            count = count.as_str(),
            section = section.as_str()
        );
    } else {
        let count = report.blockers.len().to_string();
        println!(
            "deleting {section}.{alias} is BLOCKED by {count} hard reference(s):",
            count = count.as_str(),
            section = section.as_str()
        );
        for b in &report.blockers {
            println!("  ✗ {path} (hard reference)", path = b.path.as_str());
        }
    }
    for s in &report.scrubs {
        println!("  • {path} (would be scrubbed)", path = s.path.as_str());
    }
}

/// Delete an aliased entry's config references (config-layer only).
fn delete_config(
    config: &mut Config,
    kind: &AliasKind,
    alias: &str,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let section = section_path(kind);
    if dry_run {
        print_impact(kind, alias, config);
        return Ok(());
    }
    if !yes {
        print_impact(kind, alias, config);
        println!("\nNo changes made. Re-run with --yes to apply (or --dry-run to preview).");
        return Ok(());
    }
    apply_delete(config, kind, alias)
}

/// Apply the config-layer delete (scrub refs + remove entry) and mark the dirty
/// paths. Bails on a hard-ref refusal or a missing alias. The caller persists.
fn apply_delete(config: &mut Config, kind: &AliasKind, alias: &str) -> Result<()> {
    let section = section_path(kind);
    match alias_refs::delete_with_cascade(config, kind, alias, CascadePolicy::RefuseOnHard) {
        Ok(report) => {
            for path in report.dirty_paths() {
                config.mark_dirty(&path);
            }
            let count = report.applied.len().to_string();
            println!(
                "deleted {section}.{alias} (scrubbed {count} reference(s))",
                count = count.as_str(),
                section = section.as_str()
            );
            Ok(())
        }
        Err(CascadeError::Refused(report)) => {
            let count = report.blockers.len().to_string();
            println!(
                "refused: {count} hard reference(s) block the delete:",
                count = count.as_str()
            );
            for b in &report.blockers {
                println!("  ✗ {}", b.path);
            }
            bail!("delete refused — resolve the hard references first");
        }
        Err(CascadeError::NotFound(p)) => {
            bail!("{path} is not configured", path = p.as_str())
        }
        Err(e) => {
            let es = e.to_string();
            bail!("delete failed: {error}", error = es.as_str())
        }
    }
}

/// Rename an aliased entry's config references (config-layer only).
fn rename_config(config: &mut Config, kind: &AliasKind, from: &str, to: &str) -> Result<()> {
    let report = alias_refs::rename_with_cascade(config, kind, from, to).map_err(rename_error)?;
    for path in &report.dirty_paths {
        config.mark_dirty(path);
    }
    print_renamed(kind, from, to, report.dirty_paths.len());
    Ok(())
}

fn print_renamed(kind: &AliasKind, from: &str, to: &str, dirty_paths: usize) {
    let section = section_path(kind);
    let count = dirty_paths.to_string();
    println!(
        "renamed {section}.{from} → {section}.{to} (rewrote {count} reference path(s))",
        count = count.as_str(),
        section = section.as_str()
    );
}

fn rename_error(error: RenameError) -> anyhow::Error {
    let message = match error {
        RenameError::NotFound(path) => format!("{path} is not configured", path = path.as_str()),
        RenameError::InvalidName(message) => {
            format!("invalid new alias: {message}", message = message.as_str())
        }
        RenameError::Reserved(alias) => format!(
            "alias `{alias}` is reserved and cannot be renamed",
            alias = alias.as_str()
        ),
        RenameError::PostCondition(message) => format!(
            "rename cascade post-condition failed: {message}",
            message = message.as_str()
        ),
    };
    anyhow::Error::msg(message)
}

async fn save(config: &mut Config) -> Result<()> {
    Box::pin(config.save_dirty())
        .await
        .context("failed to persist config")
}

// ── agents ──────────────────────────────────────────────────────────────────

pub async fn handle_agents(cmd: AgentsCommands, config: &mut Config) -> Result<()> {
    match cmd {
        AgentsCommands::List => list_section(config, "agents"),
        AgentsCommands::Create { alias } => {
            // A refused create must not leave an in-memory alias that would
            // make its retry skip the new-agent identity invalidation.
            let mut working = config.clone();
            let created = create_entry(&mut working, "agents", &alias)?;
            // Same fail-closed lifecycle rule as the HTTP create paths
            // (#567/#569): a NEW agent drops any stale preserved-rename
            // identity mapping BEFORE the config is saved, so a failure
            // aborts the create with nothing persisted instead of leaving a
            // saved agent bound to the old identity's durable task list. A
            // duplicate create is a no-op on the existing agent's identity —
            // `created` is false there.
            if created {
                let store_dir = working
                    .companion_memory
                    .resolved_store_dir(&working.data_dir);
                reiclaw_memory::companion::invalidate_alias_identity(&store_dir, &alias)
                    .context("agent not created: failed to clear a stale companion identity mapping (repair or remove the companion identity file and retry)")?;
            }
            save(&mut working).await?;
            *config = working;
            Ok(())
        }
        AgentsCommands::Rename { from, to } => rename_agent(config, &from, &to).await,
        AgentsCommands::Delete {
            alias,
            dry_run,
            yes,
        } => {
            if alias_refs::is_reserved_agent_alias(&alias) {
                bail!("the `default` agent is reserved and cannot be deleted");
            }
            if dry_run {
                print_impact(&AliasKind::Agent, &alias, config);
                return Ok(());
            }
            if !yes {
                print_impact(&AliasKind::Agent, &alias, config);
                println!(
                    "\nNo changes made. Re-run with --yes to apply (or --dry-run to preview)."
                );
                return Ok(());
            }
            // Owned-state HARD gate (live ACP sessions) runs BEFORE the config
            // cascade so a refusal mutates nothing.
            agent_delete_precheck(config, &alias)?;
            // Resolve the workspace dir while the entry still exists (a custom
            // `workspace.path` is read off it), then apply + PERSIST the config
            // change before any irreversible owned-state side effects — so a
            // later failure can't leave the config and owned state split.
            let workspace = config.agent_workspace_dir(&alias);
            apply_delete(config, &AliasKind::Agent, &alias)?;
            save(config).await?;
            agent_delete_owned_state(config, &alias, &workspace).await
        }
    }
}

/// Memory + optional session-backend handles opened from `data_dir` for the
/// owned-state cascade.
#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
type OwnedStateHandles = (
    std::sync::Arc<dyn reiclaw_memory::Memory>,
    Option<std::sync::Arc<dyn reiclaw_infra::session_backend::SessionBackend>>,
);

#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
fn build_owned_state_handles(config: &Config) -> Result<OwnedStateHandles> {
    use std::sync::Arc;
    let mem: Arc<dyn reiclaw_memory::Memory> = if config.agents.is_empty() {
        Arc::new(reiclaw_memory::NoneMemory::new("none"))
    } else {
        Arc::from(
            reiclaw_memory::create_memory_from_config(config, None)
                .context("open memory backend for the owned-state cascade")?,
        )
    };
    let session_backend = if config.gateway.session_persistence {
        Some(
            reiclaw_infra::make_session_backend(&config.data_dir, &config.channels.session_backend)
                .context("open session backend for the owned-state cascade")?,
        )
    } else {
        None
    };
    Ok((mem, session_backend))
}

#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
fn agent_delete_precheck(config: &Config, alias: &str) -> Result<()> {
    // Fail closed: refuse if live ACP sessions exist, or if the store can't be
    // read to verify (mirrors the gateway delete gate).
    let live = crate::gateway::agent_owned_state::live_acp_session_count(config, alias)
        .context("could not verify live ACP sessions")?;
    if live > 0 {
        let count = live.to_string();
        bail!(
            "{count} live ACP session(s) for `{alias}` — end them first",
            count = count.as_str()
        );
    }
    Ok(())
}

#[cfg(not(all(feature = "gateway", feature = "agent-runtime")))]
fn agent_delete_precheck(_config: &Config, _alias: &str) -> Result<()> {
    Ok(())
}

#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
async fn agent_delete_owned_state(
    config: &Config,
    alias: &str,
    workspace: &std::path::Path,
) -> Result<()> {
    let (mem, session_backend) = build_owned_state_handles(config)?;
    let ts = jiff::Timestamp::now().strftime("%Y%m%d%H%M%S");
    let archive_dir = config
        .data_dir
        .join("agents")
        .join("_deleted")
        .join(format!("{alias}-{ts}"));
    tokio::fs::create_dir_all(&archive_dir).await.ok();
    // Archive the workspace dir alongside the owned-state exports. `workspace`
    // was resolved by the caller before the config entry was removed, so a
    // custom `workspace.path` is preserved (post-removal it would default).
    if workspace.exists()
        && let Err(e) = tokio::fs::rename(&workspace, archive_dir.join("workspace")).await
    {
        let es = e.to_string();
        eprintln!(
            "warning: workspace archive failed: {error}",
            error = es.as_str()
        );
    }
    let report = crate::gateway::agent_owned_state::cascade_owned_state(
        config,
        &mem,
        session_backend.as_ref(),
        alias,
        &archive_dir,
    )
    .await;
    let memory = report.memory_purged.to_string();
    let cron = report.cron_removed.to_string();
    let acp = report.acp_removed.to_string();
    let sessions = report.sessions_cleared.to_string();
    let archive = archive_dir.display().to_string();
    println!(
        "owned-state cascaded: memory {memory} · cron {cron} · acp {acp} · sessions {sessions} → {archive}",
        acp = acp.as_str(),
        archive = archive.as_str(),
        cron = cron.as_str(),
        memory = memory.as_str(),
        sessions = sessions.as_str()
    );
    for w in &report.warnings {
        eprintln!("warning: {warning}", warning = w.as_str());
    }
    Ok(())
}

#[cfg(not(all(feature = "gateway", feature = "agent-runtime")))]
async fn agent_delete_owned_state(
    _config: &Config,
    _alias: &str,
    _workspace: &std::path::Path,
) -> Result<()> {
    warn_agent_owned_state();
    Ok(())
}

#[cfg(all(feature = "gateway", feature = "agent-runtime"))]
async fn rename_agent(config: &mut Config, from: &str, to: &str) -> Result<()> {
    use crate::gateway::agent_owned_state::{AgentRenameError, rename_agent_with_commit};

    // Opening the followers before committing makes a backend-open failure
    // fail closed instead of leaving a saved rename that this CLI cannot finish.
    // The normal memory factory also runs retention and light-sleep governance.
    // Disable that maintenance only for this opening; backend, storage routes,
    // scanning/audit policy and the config that will be committed stay intact.
    let (mem, session_backend) = {
        let mut opening = config.clone();
        opening.memory.hygiene_enabled = false;
        build_owned_state_handles(&opening)?
    };
    let report = Box::pin(rename_agent_with_commit(
        config.clone(),
        &mem,
        session_backend.as_ref(),
        from,
        to,
        |mut working, persist| async move {
            if persist {
                save(&mut working).await?;
                *config = working.clone();
            }
            Ok::<_, anyhow::Error>(working)
        },
    ))
    .await
    .map_err(|error| match error {
        AgentRenameError::Config(error) => rename_error(error),
        AgentRenameError::Identity(error) | AgentRenameError::Commit(error) => error,
    })?;

    print_renamed(&AliasKind::Agent, from, to, report.dirty_paths);
    let memory = report.owned.memory_rows.to_string();
    let cron = report.owned.cron_jobs.to_string();
    let acp = report.owned.acp_sessions.to_string();
    let sessions = report.owned.sessions_repointed.to_string();
    println!(
        "owned-state re-pointed: memory {memory} · cron {cron} · acp {acp} · sessions {sessions}",
        acp = acp.as_str(),
        cron = cron.as_str(),
        memory = memory.as_str(),
        sessions = sessions.as_str()
    );
    for warning in &report.owned.warnings {
        eprintln!("warning: {warning}", warning = warning.as_str());
    }
    Ok(())
}

#[cfg(not(all(feature = "gateway", feature = "agent-runtime")))]
async fn rename_agent(config: &mut Config, from: &str, to: &str) -> Result<()> {
    let mut working = config.clone();
    rename_config(&mut working, &AliasKind::Agent, from, to)?;
    let store_dir = working
        .companion_memory
        .resolved_store_dir(&working.data_dir);
    reiclaw_memory::companion::rename_alias_identity(&store_dir, from, to)?;
    save(&mut working).await?;
    *config = working;
    warn_agent_owned_state();
    Ok(())
}

#[cfg(not(all(feature = "gateway", feature = "agent-runtime")))]
fn warn_agent_owned_state() {
    eprintln!(
        "note: config references were updated, but the agent's owned state (memory rows, workspace dir, cron/acp/session rows) was NOT cascaded by this CLI yet — use the gateway API for the full owned-state cascade."
    );
}

// ── providers ─────────────────────────────────────────────────────────────────

pub async fn handle_providers(cmd: ProvidersCommands, config: &mut Config) -> Result<()> {
    match cmd {
        ProvidersCommands::List { category } => {
            let cats = match category {
                Some(c) => vec![parse_provider_category(&c)?],
                None => vec![
                    ProviderCategory::Models,
                    ProviderCategory::Tts,
                    ProviderCategory::Transcription,
                ],
            };
            for cat in cats {
                let cat_name = match cat {
                    ProviderCategory::Models => "models",
                    ProviderCategory::Tts => "tts",
                    ProviderCategory::Transcription => "transcription",
                };
                // Enumerate families under this category, then their aliases.
                if let Some(families) = config.get_map_keys(&format!("providers.{cat_name}")) {
                    let mut families = families;
                    families.sort();
                    for family in families {
                        if let Some(mut aliases) =
                            config.get_map_keys(&format!("providers.{cat_name}.{family}"))
                        {
                            aliases.sort();
                            for a in aliases {
                                println!("{cat_name}.{family}.{a}");
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        ProvidersCommands::Create {
            category,
            family,
            alias,
        } => {
            let cat = parse_provider_category(&category)?;
            let section = section_path(&AliasKind::Provider {
                category: cat,
                family,
            });
            create_entry(config, &section, &alias)?;
            save(config).await
        }
        ProvidersCommands::Rename {
            category,
            family,
            from,
            to,
        } => {
            let category = parse_provider_category(&category)?;
            rename_config(
                config,
                &AliasKind::Provider { category, family },
                &from,
                &to,
            )?;
            save(config).await
        }
        ProvidersCommands::Delete {
            category,
            family,
            alias,
            dry_run,
            yes,
        } => {
            let category = parse_provider_category(&category)?;
            let kind = AliasKind::Provider { category, family };
            delete_config(config, &kind, &alias, dry_run, yes)?;
            if yes && !dry_run {
                save(config).await?;
            }
            Ok(())
        }
    }
}

// ── channels ─────────────────────────────────────────────────────────────────

pub async fn handle_channels(cmd: ChannelsCommands, config: &mut Config) -> Result<()> {
    match cmd {
        ChannelsCommands::List { channel_type } => {
            // `channels` is a struct of per-type maps, not one flat map, so with
            // no filter we walk the canonical channel-type list.
            let types: Vec<String> = match channel_type {
                Some(t) => vec![t],
                None => config
                    .channels
                    .channels()
                    .into_iter()
                    .map(|channel| channel.kind.to_string())
                    .collect(),
            };
            let mut types = types;
            types.sort();
            for t in types {
                if let Some(mut aliases) = config.get_map_keys(&format!("channels.{t}")) {
                    aliases.sort();
                    for a in aliases {
                        println!("{t}.{a}");
                    }
                }
            }
            Ok(())
        }
        ChannelsCommands::Create {
            channel_type,
            alias,
        } => {
            create_entry(config, &format!("channels.{channel_type}"), &alias)?;
            save(config).await
        }
        ChannelsCommands::Rename {
            channel_type,
            from,
            to,
        } => {
            rename_config(config, &AliasKind::Channel { channel_type }, &from, &to)?;
            save(config).await
        }
        ChannelsCommands::Delete {
            channel_type,
            alias,
            dry_run,
            yes,
        } => {
            let kind = AliasKind::Channel { channel_type };
            delete_config(config, &kind, &alias, dry_run, yes)?;
            if yes && !dry_run {
                save(config).await?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reiclaw_config::schema::AliasedAgentConfig;

    fn agent_test_config(tmp: &tempfile::TempDir) -> Config {
        let mut config = Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        config.memory.backend = "none".into();
        config.gateway.session_persistence = false;
        config
    }

    #[tokio::test]
    async fn agent_create_failure_retry_and_duplicate_preserve_identity_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = agent_test_config(&tmp);
        Box::pin(config.save()).await.unwrap();
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        let retired =
            reiclaw_memory::companion::resolve_or_mint_agent_identity(&store_dir, "a").unwrap();
        reiclaw_memory::companion::rename_alias_identity(&store_dir, "a", "b").unwrap();
        let identity_path =
            store_dir.join(reiclaw_config::companion::COMPANION_AGENT_IDENTITY_FILE);
        let identities_before = std::fs::read(&identity_path).unwrap();
        let config_before = std::fs::read(&config.config_path).unwrap();
        let blocker = identity_path.with_extension("json.tmp");
        std::fs::create_dir(&blocker).unwrap();

        assert!(
            Box::pin(handle_agents(
                AgentsCommands::Create { alias: "a".into() },
                &mut config
            ))
            .await
            .is_err()
        );
        assert!(!config.agents.contains_key("a"));
        assert_eq!(std::fs::read(&config.config_path).unwrap(), config_before);
        assert_eq!(std::fs::read(&identity_path).unwrap(), identities_before);

        std::fs::remove_dir(blocker).unwrap();
        Box::pin(handle_agents(
            AgentsCommands::Create { alias: "a".into() },
            &mut config,
        ))
        .await
        .unwrap();
        let fresh =
            reiclaw_memory::companion::resolve_or_mint_agent_identity(&store_dir, "a").unwrap();
        assert_ne!(fresh, retired);
        Box::pin(handle_agents(
            AgentsCommands::Create { alias: "a".into() },
            &mut config,
        ))
        .await
        .unwrap();
        assert_eq!(
            reiclaw_memory::companion::peek_agent_identity(&store_dir, "a"),
            Some(fresh)
        );
        assert_eq!(
            reiclaw_memory::companion::peek_agent_identity(&store_dir, "b"),
            Some(retired)
        );
        let disk: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config.config_path).unwrap()).unwrap();
        assert!(disk["agents"].get("a").is_some());
    }

    #[tokio::test]
    async fn agent_rename_refuses_invalid_or_unwritable_identity_then_retries_safely() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = agent_test_config(&tmp);
        for alias in ["b", "c", "occupied"] {
            config
                .agents
                .insert(alias.into(), AliasedAgentConfig::default());
        }
        Box::pin(config.save()).await.unwrap();
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        let retired =
            reiclaw_memory::companion::resolve_or_mint_agent_identity(&store_dir, "a").unwrap();
        reiclaw_memory::companion::rename_alias_identity(&store_dir, "a", "b").unwrap();
        let identity_path =
            store_dir.join(reiclaw_config::companion::COMPANION_AGENT_IDENTITY_FILE);
        let identities_before = std::fs::read(&identity_path).unwrap();
        let config_before = std::fs::read(&config.config_path).unwrap();

        for target in ["occupied", "BadName", "default", "c"] {
            assert!(
                Box::pin(handle_agents(
                    AgentsCommands::Rename {
                        from: "c".into(),
                        to: target.into(),
                    },
                    &mut config,
                ))
                .await
                .is_err(),
                "{target}"
            );
            assert!(config.agents.contains_key("c"));
            assert_eq!(std::fs::read(&identity_path).unwrap(), identities_before);
            assert_eq!(std::fs::read(&config.config_path).unwrap(), config_before);
        }

        let blocker = identity_path.with_extension("json.tmp");
        std::fs::create_dir(&blocker).unwrap();
        assert!(
            Box::pin(handle_agents(
                AgentsCommands::Rename {
                    from: "c".into(),
                    to: "a".into(),
                },
                &mut config,
            ))
            .await
            .is_err()
        );
        assert!(config.agents.contains_key("c"));
        assert!(!config.agents.contains_key("a"));
        assert_eq!(std::fs::read(&config.config_path).unwrap(), config_before);
        assert_eq!(std::fs::read(&identity_path).unwrap(), identities_before);

        std::fs::remove_dir(blocker).unwrap();
        Box::pin(handle_agents(
            AgentsCommands::Rename {
                from: "c".into(),
                to: "a".into(),
            },
            &mut config,
        ))
        .await
        .unwrap();
        let fresh = reiclaw_memory::companion::peek_agent_identity(&store_dir, "a").unwrap();
        assert_ne!(fresh, retired);
        assert_eq!(
            reiclaw_memory::companion::peek_agent_identity(&store_dir, "c"),
            Some(fresh)
        );
        assert_eq!(
            reiclaw_memory::companion::peek_agent_identity(&store_dir, "b"),
            Some(retired)
        );
        assert!(config.agents.contains_key("a"));
        assert!(!config.agents.contains_key("c"));
        let disk: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config.config_path).unwrap()).unwrap();
        assert!(disk["agents"].get("a").is_some());
        assert!(disk["agents"].get("c").is_none());
    }

    #[cfg(all(feature = "gateway", feature = "agent-runtime"))]
    fn rename_test_config(tmp: &tempfile::TempDir) -> Config {
        let mut config = agent_test_config(tmp);
        config.agents.insert(
            "from".into(),
            AliasedAgentConfig {
                risk_profile: "default".into(),
                ..Default::default()
            },
        );
        config
            .risk_profiles
            .entry("default".into())
            .or_default()
            .allowed_commands = vec!["echo".into()];
        config.runtime_profiles.entry("default".into()).or_default();
        config
    }

    #[cfg(all(feature = "gateway", feature = "agent-runtime"))]
    #[tokio::test]
    async fn agent_rename_partial_workspace_failure_retries_without_rebinding_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = rename_test_config(&tmp);
        Box::pin(config.save()).await.unwrap();
        let config_path = config.config_path.clone();
        let old_ws = config.agent_workspace_dir("from");
        let new_ws = config.agent_workspace_dir("to");
        std::fs::create_dir_all(&old_ws).unwrap();
        std::fs::write(old_ws.join("USER.md"), "retained workspace").unwrap();
        let blocker = new_ws.parent().unwrap();
        std::fs::write(blocker, "blocks workspace move").unwrap();
        reiclaw_runtime::cron::add_job(&config, "from", "* * * * *", "echo hi").unwrap();

        Box::pin(handle_agents(
            AgentsCommands::Rename {
                from: "from".into(),
                to: "to".into(),
            },
            &mut config,
        ))
        .await
        .unwrap();
        assert!(config.agents.contains_key("to"));
        assert!(!config.agents.contains_key("from"));
        assert!(old_ws.join("USER.md").exists());
        assert_eq!(
            reiclaw_runtime::cron::list_jobs_by_agent(&config, "to")
                .unwrap()
                .len(),
            1,
            "an independent follower still converges after the workspace failure"
        );
        let committed_bytes = std::fs::read(&config_path).unwrap();

        let foreign_ws = tmp.path().join("foreign/workspace");
        std::fs::create_dir_all(&foreign_ws).unwrap();
        std::fs::write(foreign_ws.join("USER.md"), "unrelated workspace").unwrap();
        assert!(
            Box::pin(handle_agents(
                AgentsCommands::Rename {
                    from: "../foreign".into(),
                    to: "to".into()
                },
                &mut config,
            ))
            .await
            .is_err()
        );
        assert!(foreign_ws.join("USER.md").exists());
        assert_eq!(std::fs::read(&config_path).unwrap(), committed_bytes);

        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        reiclaw_memory::companion::invalidate_alias_identity(&store_dir, "to").unwrap();
        let current =
            reiclaw_memory::companion::resolve_or_mint_agent_identity(&store_dir, "to").unwrap();
        assert_ne!(
            reiclaw_memory::companion::peek_agent_identity(&store_dir, "from"),
            Some(current.clone())
        );
        std::fs::remove_file(blocker).unwrap();
        // A committed retry must not persist again. This path cannot be
        // replaced by an atomic file save, but keeps the same install root.
        config.config_path = tmp.path().join("persist-blocker");
        std::fs::create_dir(&config.config_path).unwrap();
        Box::pin(handle_agents(
            AgentsCommands::Rename {
                from: "from".into(),
                to: "to".into(),
            },
            &mut config,
        ))
        .await
        .unwrap();

        assert!(!old_ws.exists());
        assert_eq!(
            std::fs::read_to_string(new_ws.join("USER.md")).unwrap(),
            "retained workspace"
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), committed_bytes);
        assert_eq!(
            reiclaw_memory::companion::peek_agent_identity(&store_dir, "to"),
            Some(current)
        );
        // Once all residue is gone, an unrelated/duplicate request is refused.
        assert!(
            Box::pin(handle_agents(
                AgentsCommands::Rename {
                    from: "from".into(),
                    to: "to".into()
                },
                &mut config,
            ))
            .await
            .is_err()
        );
    }

    #[cfg(all(feature = "gateway", feature = "agent-runtime"))]
    #[tokio::test]
    async fn agent_rename_failed_commit_keeps_workspace_and_cron_at_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = rename_test_config(&tmp);
        std::fs::create_dir_all(&config.config_path).unwrap();
        let old_ws = config.agent_workspace_dir("from");
        let new_ws = config.agent_workspace_dir("to");
        std::fs::create_dir_all(&old_ws).unwrap();
        std::fs::write(old_ws.join("USER.md"), "source workspace").unwrap();
        reiclaw_runtime::cron::add_job(&config, "from", "* * * * *", "echo hi").unwrap();
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        let identity =
            reiclaw_memory::companion::resolve_or_mint_agent_identity(&store_dir, "from").unwrap();

        assert!(
            Box::pin(handle_agents(
                AgentsCommands::Rename {
                    from: "from".into(),
                    to: "to".into()
                },
                &mut config,
            ))
            .await
            .is_err()
        );
        assert!(config.agents.contains_key("from"));
        assert!(!config.agents.contains_key("to"));
        assert!(old_ws.join("USER.md").exists());
        assert!(!new_ws.exists());
        assert_eq!(
            reiclaw_runtime::cron::list_jobs_by_agent(&config, "from")
                .unwrap()
                .len(),
            1
        );
        assert!(
            reiclaw_runtime::cron::list_jobs_by_agent(&config, "to")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reiclaw_memory::companion::peek_agent_identity(&store_dir, "from"),
            Some(identity)
        );
    }

    #[cfg(all(feature = "gateway", feature = "agent-runtime"))]
    #[tokio::test]
    async fn agent_rename_refusal_and_failed_commit_do_not_run_memory_retention() {
        for invalid_target in [true, false] {
            let tmp = tempfile::tempdir().unwrap();
            let mut config = rename_test_config(&tmp);
            config.memory.hygiene_enabled = true;
            config.memory.archive_after_days = 1;
            config.memory.purge_after_days = 1;
            config.memory.audit_retention_days = 1;
            config.memory.audit_enabled = false;
            Box::pin(config.save()).await.unwrap();
            if !invalid_target {
                std::fs::remove_file(&config.config_path).unwrap();
                std::fs::create_dir(&config.config_path).unwrap();
            }

            let retained_files = [
                "memory/2000-01-01.md",
                "memory/archive/2000-01-02.md",
                "sessions/2000-01-01.jsonl",
                "sessions/archive/2000-01-02.jsonl",
            ];
            for file in retained_files {
                let path = config.data_dir.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "retained record").unwrap();
            }
            let audit_path = config.data_dir.join("memory/audit.db");
            {
                let audit = rusqlite::Connection::open(&audit_path).unwrap();
                audit
                    .execute_batch(
                        "CREATE TABLE memory_audit (
                        id INTEGER PRIMARY KEY, operation TEXT NOT NULL, key TEXT,
                        namespace TEXT, session_id TEXT, timestamp TEXT NOT NULL,
                        metadata TEXT
                    );
                    INSERT INTO memory_audit (operation, timestamp)
                    VALUES ('store', '2000-01-01T00:00:00+00:00');",
                    )
                    .unwrap();
            }
            assert!(reiclaw_memory::hygiene::is_due(&config.data_dir).unwrap());

            assert!(
                Box::pin(handle_agents(
                    AgentsCommands::Rename {
                        from: "from".into(),
                        to: if invalid_target { "BadName" } else { "to" }.into(),
                    },
                    &mut config,
                ))
                .await
                .is_err()
            );

            assert!(config.agents.contains_key("from"));
            assert!(
                config.memory.hygiene_enabled,
                "the operator setting stays enabled"
            );
            for file in retained_files {
                assert_eq!(
                    std::fs::read_to_string(config.data_dir.join(file)).unwrap(),
                    "retained record",
                    "{file} must survive a rename that never commits"
                );
            }
            let audit = rusqlite::Connection::open(&audit_path).unwrap();
            let count: i64 = audit
                .query_row("SELECT COUNT(*) FROM memory_audit", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1);
            assert!(
                !config
                    .data_dir
                    .join("state/memory_hygiene_state.json")
                    .exists()
            );
        }
    }

    #[test]
    fn parse_provider_category_maps_known_and_rejects_unknown() {
        assert_eq!(
            parse_provider_category("models").unwrap(),
            ProviderCategory::Models
        );
        assert_eq!(
            parse_provider_category("tts").unwrap(),
            ProviderCategory::Tts
        );
        assert_eq!(
            parse_provider_category("transcription").unwrap(),
            ProviderCategory::Transcription
        );
        assert!(parse_provider_category("bogus").is_err());
    }

    #[test]
    fn section_path_for_each_kind() {
        assert_eq!(section_path(&AliasKind::Agent), "agents");
        assert_eq!(
            section_path(&AliasKind::Provider {
                category: ProviderCategory::Models,
                family: "anthropic".to_string(),
            }),
            "providers.models.anthropic"
        );
        assert_eq!(
            section_path(&AliasKind::Provider {
                category: ProviderCategory::Tts,
                family: "elevenlabs".to_string(),
            }),
            "providers.tts.elevenlabs"
        );
        assert_eq!(
            section_path(&AliasKind::Channel {
                channel_type: "telegram".to_string(),
            }),
            "channels.telegram"
        );
    }
}
