//! Transport-neutral agent rename orchestration and owned-state cascades.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use reiclaw_api::memory_traits::Memory;
use reiclaw_config::alias_refs::{self, AliasKind, RenameError};
use reiclaw_config::schema::Config;
use reiclaw_infra::acp_session_store::AcpSessionStore;
use reiclaw_infra::session_backend::SessionBackend;

pub fn live_acp_session_count(config: &Config, alias: &str) -> anyhow::Result<usize> {
    let store = AcpSessionStore::new(&config.data_dir)
        .context("open ACP session store to verify live sessions")?;
    store
        .count_live_sessions_by_agent(alias)
        .context("count live ACP sessions for agent")
}

/// What the owned-state cascade removed.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct OwnedStateReport {
    pub memory_purged: usize,
    pub cron_removed: usize,
    pub acp_removed: usize,
    pub sessions_cleared: usize,
    pub archived_to: Option<String>,
    /// Surfaced failures (export / purge / delete errors). Non-empty means part
    /// of the cascade did NOT complete — those rows were not silently treated as
    /// removed. The handler logs these; nothing is masked as success.
    pub warnings: Vec<String>,
}

async fn write_json(path: &Path, value: serde_json::Value) -> anyhow::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        reiclaw_infra::owned_archive::write_verified_json(&path, &value)
    })
    .await?
}

fn removed_or_retained(
    store: &str,
    result: anyhow::Result<usize>,
    warnings: &mut Vec<String>,
) -> usize {
    match result {
        Ok(count) => count,
        Err(error) => {
            warnings.push(format!(
                "{store} archive/purge: {error:#}; original records retained"
            ));
            0
        }
    }
}

pub async fn cascade_owned_state(
    config: &Config,
    mem: &Arc<dyn Memory>,
    session_backend: Option<&Arc<dyn SessionBackend>>,
    alias: &str,
    archive_dir: &Path,
) -> OwnedStateReport {
    let cascade_dir = archive_dir.join("cascade");
    let mut warnings: Vec<String> = Vec::new();

    // Each store owns the guarded export → verified archive → exact deletion.
    // An unsupported backend or any failure retains that store's original data.
    // There is intentionally no cross-store transaction or unguarded fallback.
    let memory_purged = removed_or_retained(
        "memory",
        mem.archive_and_purge_agent(alias, &cascade_dir.join("memory.json"))
            .await,
        &mut warnings,
    );

    let owned_config = config.clone();
    let owned_alias = alias.to_owned();
    let stores = tokio::task::spawn_blocking(move || {
        let cron = reiclaw_runtime::cron::archive_and_remove_jobs_by_agent(
            &owned_config,
            &owned_alias,
            &cascade_dir.join("cron.json"),
        );
        let acp = AcpSessionStore::new(&owned_config.data_dir).and_then(|store| {
            store.archive_and_delete_sessions_by_agent(&owned_alias, &cascade_dir.join("acp.json"))
        });
        (cron, acp)
    })
    .await;
    let (cron_removed, acp_removed) = match stores {
        Ok((cron, acp)) => (
            removed_or_retained("cron", cron, &mut warnings),
            removed_or_retained("acp", acp, &mut warnings),
        ),
        Err(error) => {
            warnings.push(format!(
                "owned-state archive worker failed: {error}; inspect each archive before retrying"
            ));
            (0, 0)
        }
    };

    // ── session metadata: clear the stale agent attribution (keep the convo) ─
    let sessions_cleared = match session_backend {
        Some(b) => match b.clear_agent_attribution(alias) {
            Ok(n) => n,
            Err(e) => {
                warnings.push(format!("session attribution clear: {e}"));
                0
            }
        },
        None => 0,
    };

    if !warnings.is_empty() {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"agent": alias, "warnings": warnings})),
            "owned-state cascade completed with warnings (some state may not have been removed)"
        );
    }

    let mut report = OwnedStateReport {
        memory_purged,
        cron_removed,
        acp_removed,
        sessions_cleared,
        archived_to: Some(archive_dir.display().to_string()),
        warnings,
    };

    // ── manifest: a self-describing record of the bundle ────────────────────
    let manifest = serde_json::json!({
        "alias": alias,
        "memory_rows": report.memory_purged,
        "cron_jobs": report.cron_removed,
        "acp_sessions": report.acp_removed,
        "sessions_cleared": report.sessions_cleared,
        "warnings": report.warnings,
    });
    if let Err(error) = write_json(&archive_dir.join("manifest.json"), manifest).await {
        report.warnings.push(format!("archive manifest: {error:#}"));
    }

    report
}

/// What the agent-rename owned-state cascade re-pointed
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RenameStateReport {
    pub memory_rows: usize,
    pub cron_jobs: usize,
    pub acp_sessions: usize,
    pub sessions_repointed: usize,
    /// Surfaced failures. Non-empty means part of the cascade did NOT complete —
    /// those rows were not silently treated as re-pointed.
    pub warnings: Vec<String>,
}

pub async fn cascade_rename_agent(
    config: &Config,
    mem: &Arc<dyn Memory>,
    session_backend: Option<&Arc<dyn SessionBackend>>,
    from: &str,
    to: &str,
) -> RenameStateReport {
    let mut warnings: Vec<String> = Vec::new();

    let memory_rows = match mem.rename_agent(from, to).await {
        Ok(n) => n,
        Err(e) => {
            warnings.push(format!("memory rename: {e}"));
            0
        }
    };

    let cron_jobs = match reiclaw_runtime::cron::rename_jobs_by_agent(config, from, to) {
        Ok(n) => n,
        Err(e) => {
            warnings.push(format!("cron rename: {e}"));
            0
        }
    };

    let acp_sessions = match AcpSessionStore::new(&config.data_dir) {
        Ok(store) => match store.rename_sessions_by_agent(from, to) {
            Ok(n) => n,
            Err(e) => {
                warnings.push(format!("acp rename: {e}"));
                0
            }
        },
        Err(e) => {
            warnings.push(format!("acp store open: {e}"));
            0
        }
    };

    // Identity is already bound by the config API/CLI before committing the
    // rename. This best-effort follower must not overwrite that authority
    // after awaits or on a retry of an older owned-state cascade.

    let sessions_repointed = match session_backend {
        Some(b) => match b.rename_agent_attribution(from, to) {
            Ok(n) => n,
            Err(e) => {
                warnings.push(format!("session attribution rename: {e}"));
                0
            }
        },
        None => 0,
    };

    if !warnings.is_empty() {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"from": from, "to": to, "warnings": warnings})),
            "rename owned-state cascade completed with warnings (some state may not have been re-pointed)"
        );
    }

    RenameStateReport {
        memory_rows,
        cron_jobs,
        acp_sessions,
        sessions_repointed,
        warnings,
    }
}

/// A failed rename has not reached its owned-state followers. A commit adapter
/// retains its own persistence error type so HTTP and CLI keep their contracts.
#[derive(Debug)]
pub enum AgentRenameError<CommitError> {
    Config(RenameError),
    Identity(anyhow::Error),
    Commit(CommitError),
}

#[derive(Debug)]
pub struct AgentRenameReport {
    pub dirty_paths: usize,
    /// Includes workspace failures as well as the individual store failures.
    pub owned: RenameStateReport,
}

/// Rename once, or resume the followers of an already-committed rename.
///
/// The adapter commits the prepared config when `persist` is true and returns
/// its committed snapshot. It must finish any live swap and release its config
/// write guard before returning, including a retry where `persist` is false.
/// Identity binds before that boundary; workspace and store changes follow it.
pub async fn rename_agent_with_commit<Commit, CommitFuture, CommitError>(
    mut working: Config,
    mem: &Arc<dyn Memory>,
    session_backend: Option<&Arc<dyn SessionBackend>>,
    from: &str,
    to: &str,
    commit: Commit,
) -> Result<AgentRenameReport, AgentRenameError<CommitError>>
where
    Commit: FnOnce(Config, bool) -> CommitFuture,
    CommitFuture: std::future::Future<Output = Result<Config, CommitError>>,
{
    // A retry skips the config rewrite, but never its reserved/name guards.
    if alias_refs::is_reserved_agent_alias(from) || alias_refs::is_reserved_agent_alias(to) {
        return Err(AgentRenameError::Config(RenameError::Reserved(
            "default".to_string(),
        )));
    }
    reiclaw_config::helpers::validate_alias_key(to)
        .map_err(|error| AgentRenameError::Config(RenameError::InvalidName(error)))?;

    let committed_to = working.agent(from).is_none() && working.agent(to).is_some();
    if committed_to {
        // An absent source cannot supply a configured workspace. Reject path
        // syntax before treating its derived per-alias directory as residue.
        reiclaw_config::helpers::validate_alias_key(from)
            .map_err(|error| AgentRenameError::Config(RenameError::InvalidName(error)))?;
    }
    // Resolve custom paths before the source entry is moved.
    let old_ws = working.agent_workspace_dir(from);
    let resumed = committed_to && rename_residue_exists(&working, mem, session_backend, from).await;
    let dirty_paths = if resumed {
        0
    } else {
        let report = alias_refs::rename_with_cascade(&mut working, &AliasKind::Agent, from, to)
            .map_err(AgentRenameError::Config)?;
        for path in &report.dirty_paths {
            working.mark_dirty(path);
        }
        let store_dir = working
            .companion_memory
            .resolved_store_dir(&working.data_dir);
        // This preserves the source mapping if config persistence fails. A
        // post-commit retry must never rebind the target's current identity.
        reiclaw_memory::companion::rename_alias_identity(&store_dir, from, to)
            .map_err(AgentRenameError::Identity)?;
        report.dirty_paths.len()
    };

    let config = commit(working, !resumed)
        .await
        .map_err(AgentRenameError::Commit)?;
    let new_ws = config.agent_workspace_dir(to);
    let ws_existed = old_ws != new_ws && old_ws.exists();
    let move_warning = move_renamed_workspace(&old_ws, &new_ws).await;
    let workspace_moved = ws_existed && move_warning.is_none();
    let mut owned = cascade_rename_agent(&config, mem, session_backend, from, to).await;
    if let Some(warning) = move_warning {
        owned.warnings.insert(0, warning);
    }

    if owned.warnings.is_empty() {
        ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"from": from, "to": to, "memory": owned.memory_rows, "cron": owned.cron_jobs, "acp": owned.acp_sessions, "sessions": owned.sessions_repointed, "workspace_moved": workspace_moved, "dirty_paths": dirty_paths})), "agent renamed with owned-state cascade");
    } else {
        ::reiclaw_log::record!(WARN, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"from": from, "to": to, "memory": owned.memory_rows, "cron": owned.cron_jobs, "acp": owned.acp_sessions, "sessions": owned.sessions_repointed, "workspace_moved": workspace_moved, "dirty_paths": dirty_paths, "warnings": owned.warnings})), "agent rename persisted but a post-persist side-effect did not follow; re-issue the rename to converge");
    }
    Ok(AgentRenameReport { dirty_paths, owned })
}

async fn rename_residue_exists(
    config: &Config,
    mem: &Arc<dyn Memory>,
    session_backend: Option<&Arc<dyn SessionBackend>>,
    from: &str,
) -> bool {
    // With the source absent, this is its default per-alias directory. Custom
    // alias-independent workspaces are never moved and are not rename residue.
    if config.agent_workspace_dir(from).exists() {
        return true;
    }
    if reiclaw_runtime::cron::list_jobs_by_agent(config, from)
        .map(|jobs| !jobs.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    if let Ok(store) = AcpSessionStore::new(&config.data_dir)
        && store
            .list_sessions_by_agent(from)
            .map(|sessions| !sessions.is_empty())
            .unwrap_or(false)
    {
        return true;
    }
    if mem.count_agent(from).await.unwrap_or(0) > 0 {
        return true;
    }
    session_backend.is_some_and(|backend| backend.count_agent_attribution(from).unwrap_or(0) > 0)
}

async fn move_renamed_workspace(old_ws: &Path, new_ws: &Path) -> Option<String> {
    if old_ws == new_ws || !old_ws.exists() {
        return None;
    }
    if let Some(parent) = new_ws.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match tokio::fs::rename(old_ws, new_ws).await {
        Ok(()) => None,
        Err(err) => {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "old": old_ws.display().to_string(),
                        "new": new_ws.display().to_string(),
                        "err": err.to_string()
                    })),
                "agent rename: workspace move failed"
            );
            Some(format!(
                "workspace move {} -> {} failed: {err}",
                old_ws.display(),
                new_ws.display()
            ))
        }
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[tokio::test]
    async fn renamed_workspace_move_failure_is_surfaced() {
        // A failed workspace move during rename must surface a warning (so the
        // caller learns config/DB moved to `to` while the workspace is stranded
        // at `from`), not be swallowed as a clean success.
        let tmp = tempfile::tempdir().unwrap();
        let old_ws = tmp.path().join("from-ws");
        std::fs::create_dir_all(&old_ws).unwrap();
        // Force the move to fail: new_ws's parent is a FILE, so create_dir_all
        // and rename both fail.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let new_ws = blocker.join("to-ws");

        let warning = move_renamed_workspace(&old_ws, &new_ws).await;
        assert!(
            warning.is_some(),
            "a failed workspace move must surface a warning"
        );
        assert!(warning.unwrap().contains("workspace move"));
        assert!(old_ws.exists(), "source dir stays put when the move fails");

        // Nothing-to-move paths return None (no spurious warning).
        assert!(move_renamed_workspace(&old_ws, &old_ws).await.is_none());
        let missing = tmp.path().join("does-not-exist");
        assert!(move_renamed_workspace(&missing, &new_ws).await.is_none());
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    use reiclaw_api::memory_traits::MemoryCategory;
    use reiclaw_config::schema::MemoryConfig;
    use reiclaw_memory::{TACHI_COMPILED, create_memory};

    #[tokio::test]
    async fn failed_archive_keeps_memory_and_reports_independent_store_results() {
        if !TACHI_COMPILED {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: directory.path().join("data"),
            ..Config::default()
        };
        let mem_cfg = MemoryConfig {
            backend: "tachi".into(),
            hygiene_enabled: false,
            ..MemoryConfig::default()
        };
        let memory: Arc<dyn Memory> =
            Arc::from(create_memory(&mem_cfg, &config.data_dir, None).unwrap());
        let agent_id = memory.ensure_agent_uuid("alpha").await.unwrap();
        memory
            .store_with_agent(
                "note",
                "must survive",
                MemoryCategory::Core,
                None,
                None,
                None,
                Some(&agent_id),
            )
            .await
            .unwrap();
        let archive = directory.path().join("archive");
        std::fs::create_dir_all(archive.join("cascade")).unwrap();
        std::fs::create_dir(archive.join("cascade/memory.json")).unwrap();
        let report = cascade_owned_state(&config, &memory, None, "alpha", &archive).await;
        assert_eq!(report.memory_purged, 0);
        assert_eq!(
            memory.export_agent("alpha").await.unwrap()[0].content,
            "must survive"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("memory archive/purge"))
        );
        assert!(archive.join("cascade/cron.json").is_file());
        assert!(archive.join("cascade/acp.json").is_file());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["memory_rows"], 0);
        assert!(!manifest["warnings"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsupported_memory_backend_reports_retention_without_purge_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: directory.path().join("data"),
            ..Config::default()
        };
        let memory: Arc<dyn Memory> = Arc::new(reiclaw_memory::none::NoneMemory::new("none"));
        let report = cascade_owned_state(
            &config,
            &memory,
            None,
            "alpha",
            &directory.path().join("archive"),
        )
        .await;
        assert_eq!(report.memory_purged, 0);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("unsupported") && warning.contains("retained"))
        );
    }
}
