use crate::security::otp::OtpValidator;
use anyhow::{Context, Result};
use reiclaw_config::domain_matcher::DomainMatcher;
use reiclaw_config::schema::EstopConfig;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

pub use reiclaw_policy::estop::{
    EstopHandle, EstopSnapshot, EstopState, ensure_initialized_marker, has_initialized_marker,
    normalize_tool_name, now_rfc3339, parse_estop_state_strict, resolve_state_file_path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EstopLevel {
    KillAll,
    NetworkKill,
    DomainBlock(Vec<String>),
    ToolFreeze(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeSelector {
    KillAll,
    Network,
    Domains(Vec<String>),
    Tools(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct EstopManager {
    config: EstopConfig,
    state_path: PathBuf,
    lock_path: PathBuf,
    state: EstopState,
    fail_closed_reason: Option<String>,
}

fn lock_path_for(state_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", state_path.display()))
}

fn read_state_no_writeback(state_path: &Path) -> (EstopState, Option<String>) {
    let lstat_meta = match fs::symlink_metadata(state_path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return match has_initialized_marker(state_path) {
                Ok(false) => (EstopState::default(), None),
                Ok(true) => {
                    let reason = format!(
                        "Estop state file {} disappeared after initialization; entering fail-closed mode",
                        state_path.display()
                    );
                    (EstopState::fail_closed(), Some(reason))
                }
                Err(marker_err) => {
                    let reason = format!(
                        "Failed to verify estop initialized marker for {}: {marker_err}",
                        state_path.display()
                    );
                    (EstopState::fail_closed(), Some(reason))
                }
            };
        }
        Err(error) => {
            let reason = format!(
                "Failed to stat estop state file {}; entering fail-closed mode: {error}",
                state_path.display()
            );
            return (EstopState::fail_closed(), Some(reason));
        }
    };

    if lstat_meta.file_type().is_symlink() || !lstat_meta.file_type().is_file() {
        let reason = format!(
            "Estop state path {} is a symlink or non-regular file; entering fail-closed mode",
            state_path.display()
        );
        return (EstopState::fail_closed(), Some(reason));
    }

    let mut open_opts = OpenOptions::new();
    open_opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match open_opts.open(state_path) {
        Ok(file) => file,
        Err(error) => {
            let reason = format!(
                "Failed to open estop state file {}; entering fail-closed mode: {error}",
                state_path.display()
            );
            return (EstopState::fail_closed(), Some(reason));
        }
    };
    let fstat_meta = match file.metadata() {
        Ok(meta) => meta,
        Err(error) => {
            let reason = format!(
                "Failed to inspect estop state file {}; entering fail-closed mode: {error}",
                state_path.display()
            );
            return (EstopState::fail_closed(), Some(reason));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !fstat_meta.file_type().is_file()
            || fstat_meta.dev() != lstat_meta.dev()
            || fstat_meta.ino() != lstat_meta.ino()
        {
            let reason = format!(
                "Estop state file {} changed identity during read; entering fail-closed mode",
                state_path.display()
            );
            return (EstopState::fail_closed(), Some(reason));
        }
    }
    #[cfg(not(unix))]
    if !fstat_meta.file_type().is_file() {
        let reason = format!(
            "Estop state file {} is not a regular file; entering fail-closed mode",
            state_path.display()
        );
        return (EstopState::fail_closed(), Some(reason));
    }

    let mut raw = String::new();
    match file.read_to_string(&mut raw) {
        Ok(_) => match parse_estop_state_strict(&raw) {
            Ok(parsed) => {
                if !matches!(
                    reiclaw_policy::estop::has_initialized_marker(state_path),
                    Ok(true)
                ) {
                    // Pre-upgrade estop-state.json may lack the durable marker; backfill it
                    // on load so subsequent disappearance still fails closed.
                    let _ = reiclaw_policy::estop::ensure_initialized_marker(state_path);
                }
                (parsed, None)
            }
            Err(error) => {
                let reason = format!(
                    "Failed to parse estop state file {}; entering fail-closed mode: {error}",
                    state_path.display()
                );
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "path": state_path.display().to_string(),
                            "error": format!("{error}"),
                        })),
                    "Failed to parse estop state file; entering fail-closed mode: "
                );
                (EstopState::fail_closed(), Some(reason))
            }
        },
        Err(error) => {
            let reason = format!(
                "Failed to read estop state file {}; entering fail-closed mode: {error}",
                state_path.display()
            );
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "path": state_path.display().to_string(),
                        "error": format!("{error}"),
                    })),
                "Failed to read estop state file; entering fail-closed mode: "
            );
            (EstopState::fail_closed(), Some(reason))
        }
    }
}

impl EstopManager {
    pub fn load(config: &EstopConfig, config_dir: &Path) -> Result<Self> {
        let state_path = resolve_state_file_path(config_dir, &config.state_file);
        let lock_path = lock_path_for(&state_path);
        let (mut state, fail_closed_reason) = read_state_no_writeback(&state_path);
        state.normalize();

        Ok(Self {
            config: config.clone(),
            state_path,
            lock_path,
            state,
            fail_closed_reason,
        })
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn status(&self) -> EstopState {
        self.state.clone()
    }

    pub fn fail_closed_reason(&self) -> Option<&str> {
        self.fail_closed_reason.as_deref()
    }

    fn with_file_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create estop lock dir {}",
                    parent.display().to_string()
                )
            })?;
        }
        let existing_lstat = match fs::symlink_metadata(&self.lock_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                    anyhow::bail!(
                        "Refusing symlink or non-regular estop lock file {}",
                        self.lock_path.display()
                    );
                }
                Some(meta)
            }
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "Failed to stat estop lock file {}",
                        self.lock_path.display()
                    )
                });
            }
        };
        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let lock_file = opts.open(&self.lock_path).with_context(|| {
            format!(
                "Failed to open estop lock file {}",
                self.lock_path.display()
            )
        })?;
        let fstat_meta = lock_file.metadata().with_context(|| {
            format!(
                "Failed to inspect estop lock file {}",
                self.lock_path.display()
            )
        })?;
        if !fstat_meta.file_type().is_file() {
            anyhow::bail!(
                "Refusing non-regular estop lock file {}",
                self.lock_path.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if let Some(lstat) = existing_lstat
                && (fstat_meta.dev() != lstat.dev() || fstat_meta.ino() != lstat.ino())
            {
                anyhow::bail!(
                    "Estop lock file {} changed identity during open",
                    self.lock_path.display()
                );
            }
            lock_file
                .set_permissions(fs::Permissions::from_mode(0o600))
                .with_context(|| {
                    format!(
                        "Failed to set permissions on estop lock file {}",
                        self.lock_path.display()
                    )
                })?;
        }
        lock_file.lock().with_context(|| {
            format!(
                "Failed to acquire exclusive estop lock {}",
                self.lock_path.display()
            )
        })?;
        f()
    }

    pub fn engage(&mut self, level: EstopLevel) -> Result<()> {
        // Validate arguments before acquiring the lock or mutating state.
        let validated = match level {
            EstopLevel::KillAll => EstopLevel::KillAll,
            EstopLevel::NetworkKill => EstopLevel::NetworkKill,
            EstopLevel::DomainBlock(domains) => {
                let mut out = Vec::with_capacity(domains.len());
                for domain in domains {
                    let normalized = domain.trim().to_ascii_lowercase();
                    DomainMatcher::validate_pattern(&normalized)?;
                    out.push(normalized);
                }
                EstopLevel::DomainBlock(out)
            }
            EstopLevel::ToolFreeze(tools) => {
                let mut out = Vec::with_capacity(tools.len());
                for tool in tools {
                    out.push(normalize_tool_name(&tool)?);
                }
                EstopLevel::ToolFreeze(out)
            }
        };

        let state_path = self.state_path.clone();
        let updated = self.with_file_lock(|| {
            let (mut current, reason) = read_state_no_writeback(&state_path);
            if reason.is_some() {
                current.kill_all = true;
            }
            match validated {
                EstopLevel::KillAll => current.kill_all = true,
                EstopLevel::NetworkKill => current.network_kill = true,
                EstopLevel::DomainBlock(domains) => current.blocked_domains.extend(domains),
                EstopLevel::ToolFreeze(tools) => current.frozen_tools.extend(tools),
            }
            current.updated_at = Some(now_rfc3339());
            current.normalize();
            persist_state_at(&state_path, &current)?;
            Ok(current)
        })?;

        self.state = updated;
        self.fail_closed_reason = None;
        Ok(())
    }

    pub fn resume(
        &mut self,
        selector: ResumeSelector,
        otp_code: Option<&str>,
        otp_validator: Option<&OtpValidator>,
    ) -> Result<()> {
        self.ensure_resume_is_authorized(otp_code, otp_validator)?;

        let normalized_selector = match selector {
            ResumeSelector::KillAll => ResumeSelector::KillAll,
            ResumeSelector::Network => ResumeSelector::Network,
            ResumeSelector::Domains(domains) => ResumeSelector::Domains(
                domains
                    .iter()
                    .map(|domain| domain.trim().to_ascii_lowercase())
                    .collect(),
            ),
            ResumeSelector::Tools(tools) => ResumeSelector::Tools(
                tools
                    .iter()
                    .map(|tool| normalize_tool_name(tool))
                    .collect::<Result<Vec<_>>>()?,
            ),
        };

        let state_path = self.state_path.clone();
        let updated = self.with_file_lock(|| {
            let (mut current, reason) = read_state_no_writeback(&state_path);
            if let Some(reason) = reason {
                anyhow::bail!("Refusing to resume unreadable/corrupted estop state: {reason}");
            }
            match normalized_selector {
                ResumeSelector::KillAll => {
                    current.kill_all = false;
                }
                ResumeSelector::Network => {
                    current.network_kill = false;
                }
                ResumeSelector::Domains(domains) => {
                    current
                        .blocked_domains
                        .retain(|existing| !domains.iter().any(|target| target == existing));
                }
                ResumeSelector::Tools(tools) => {
                    current
                        .frozen_tools
                        .retain(|existing| !tools.iter().any(|target| target == existing));
                }
            }
            current.updated_at = Some(now_rfc3339());
            current.normalize();
            persist_state_at(&state_path, &current)?;
            Ok(current)
        })?;

        self.state = updated;
        self.fail_closed_reason = None;
        Ok(())
    }

    fn ensure_resume_is_authorized(
        &self,
        otp_code: Option<&str>,
        otp_validator: Option<&OtpValidator>,
    ) -> Result<()> {
        if !self.config.require_otp_to_resume {
            return Ok(());
        }

        let code = otp_code
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("OTP code is required to resume estop state")?;
        let validator = otp_validator
            .context("OTP validator is required to resume estop state with OTP enabled")?;
        let valid = validator.validate(code)?;
        if !valid {
            anyhow::bail!("Invalid OTP code; estop resume denied");
        }
        Ok(())
    }
}

fn persist_state_at(state_path: &Path, state: &EstopState) -> Result<()> {
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create estop state dir {}",
                parent.display().to_string()
            )
        })?;
    }

    let body = serde_json::to_string_pretty(state).context("Failed to serialize estop state")?;

    let temp_path = state_path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    fs::write(&temp_path, body).with_context(|| {
        format!(
            "Failed to write temporary estop state file {}",
            temp_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!(
                "Failed to set permissions on temporary estop state file {}",
                temp_path.display()
            )
        })?;
    }

    fs::rename(&temp_path, state_path).with_context(|| {
        format!(
            "Failed to atomically replace estop state file {}",
            state_path.display()
        )
    })?;
    ensure_initialized_marker(state_path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecretStore;
    use crate::security::otp::OtpValidator;
    use reiclaw_config::schema::OtpConfig;
    use tempfile::tempdir;

    fn estop_config(path: &Path) -> EstopConfig {
        EstopConfig {
            enabled: true,
            state_file: path.display().to_string(),
            require_otp_to_resume: false,
        }
    }

    #[test]
    fn estop_levels_compose_and_resume() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();

        manager
            .engage(EstopLevel::DomainBlock(vec!["*.chase.com".into()]))
            .unwrap();
        manager
            .engage(EstopLevel::ToolFreeze(vec![
                "shell".into(),
                "node:phone:system.info".into(),
            ]))
            .unwrap();
        manager.engage(EstopLevel::NetworkKill).unwrap();
        assert!(manager.status().network_kill);
        assert_eq!(manager.status().blocked_domains, vec!["*.chase.com"]);
        assert_eq!(
            manager.status().frozen_tools,
            vec!["node:phone:system.info", "shell"]
        );

        manager
            .resume(
                ResumeSelector::Domains(vec!["*.chase.com".into()]),
                None,
                None,
            )
            .unwrap();
        assert!(manager.status().blocked_domains.is_empty());
        assert!(manager.status().network_kill);

        manager
            .resume(ResumeSelector::Tools(vec!["shell".into()]), None, None)
            .unwrap();
        assert_eq!(
            manager.status().frozen_tools,
            vec!["node:phone:system.info"]
        );
    }

    #[test]
    fn estop_state_survives_reload() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);

        {
            let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
            manager.engage(EstopLevel::KillAll).unwrap();
            manager
                .engage(EstopLevel::DomainBlock(vec!["*.paypal.com".into()]))
                .unwrap();
        }

        let reloaded = EstopManager::load(&cfg, dir.path()).unwrap();
        let state = reloaded.status();
        assert!(state.kill_all);
        assert_eq!(state.blocked_domains, vec!["*.paypal.com"]);
    }

    #[test]
    fn corrupted_state_defaults_to_fail_closed_kill_all_without_overwriting_disk() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        fs::write(&state_path, "{not-valid-json").unwrap();
        let cfg = estop_config(&state_path);
        let manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(manager.status().kill_all);
        assert!(manager.fail_closed_reason().is_some());
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "{not-valid-json");
    }

    #[test]
    fn concurrent_engage_preserves_independent_restrictions() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);

        // Two managers load from empty initial state before either engages.
        let mut mgr_a = EstopManager::load(&cfg, dir.path()).unwrap();
        let mut mgr_b = EstopManager::load(&cfg, dir.path()).unwrap();

        let t1 = std::thread::spawn(move || {
            mgr_a.engage(EstopLevel::NetworkKill).unwrap();
        });
        let t2 = std::thread::spawn(move || {
            mgr_b
                .engage(EstopLevel::ToolFreeze(vec!["shell".into()]))
                .unwrap();
        });
        t1.join().unwrap();
        t2.join().unwrap();

        let final_state = EstopManager::load(&cfg, dir.path()).unwrap().status();
        assert!(final_state.network_kill);
        assert_eq!(final_state.frozen_tools, vec!["shell"]);
    }

    #[test]
    fn resume_requires_valid_otp_when_enabled() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let mut cfg = estop_config(&state_path);
        cfg.require_otp_to_resume = true;

        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        manager.engage(EstopLevel::KillAll).unwrap();

        let err = manager
            .resume(ResumeSelector::KillAll, None, None)
            .expect_err("resume should require OTP");
        assert!(err.to_string().contains("OTP code is required"));
    }

    #[test]
    fn resume_accepts_valid_otp_code() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let mut cfg = estop_config(&state_path);
        cfg.require_otp_to_resume = true;

        let otp_cfg = OtpConfig {
            enabled: true,
            ..OtpConfig::default()
        };
        let store = SecretStore::new(dir.path(), true);
        let (validator, _) = OtpValidator::from_config(&otp_cfg, dir.path(), &store).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let code = validator.code_for_timestamp(now);

        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        manager.engage(EstopLevel::KillAll).unwrap();
        manager
            .resume(ResumeSelector::KillAll, Some(&code), Some(&validator))
            .unwrap();
        assert!(!manager.status().kill_all);
    }

    #[test]
    fn deleted_state_after_init_fails_closed_even_through_weaker_engage() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);

        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        manager.engage(EstopLevel::NetworkKill).unwrap();
        assert!(reiclaw_policy::estop::initialized_marker_path(&state_path).exists());

        // Delete the primary state file after initialization.
        fs::remove_file(&state_path).unwrap();

        // Fresh read-only EstopHandle sees .initialized and fails closed.
        let handle = EstopHandle::for_path(state_path.clone());
        assert!(handle.enforce_not_killed().is_err());

        // A weaker engage must preserve kill_all = true until explicit resume.
        let mut second_mgr = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(second_mgr.status().kill_all);
        second_mgr
            .engage(EstopLevel::ToolFreeze(vec!["shell".into()]))
            .unwrap();
        assert!(second_mgr.status().kill_all);
        assert_eq!(second_mgr.status().frozen_tools, vec!["shell"]);

        second_mgr
            .resume(ResumeSelector::KillAll, None, None)
            .unwrap();
        assert!(!second_mgr.status().kill_all);
        assert!(handle.enforce_not_killed().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_at_lock_path_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let target = dir.path().join("innocent-target");
        fs::write(&target, "unchanged").unwrap();
        symlink(&target, lock_path_for(&state_path)).unwrap();

        let cfg = estop_config(&state_path);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        let err = manager
            .engage(EstopLevel::KillAll)
            .expect_err("symlink at .lock must be rejected");
        assert!(err.to_string().contains("symlink"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "unchanged");
    }

    #[test]
    fn pre_upgrade_state_without_marker_backfills_marker_and_fails_closed_on_disappearance() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let legacy_json = serde_json::to_string_pretty(&EstopState::default()).unwrap();
        fs::write(&state_path, legacy_json).unwrap();
        assert!(!reiclaw_policy::estop::initialized_marker_path(&state_path).exists());

        // Observing via EstopHandle establishes the durable marker.
        let handle = EstopHandle::for_path(state_path.clone());
        assert!(handle.enforce_not_killed().is_ok());
        assert!(reiclaw_policy::estop::initialized_marker_path(&state_path).exists());

        // Delete state file after observation, then try a weaker engage.
        fs::remove_file(&state_path).unwrap();
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(manager.status().kill_all);
        manager
            .engage(EstopLevel::ToolFreeze(vec!["shell".into()]))
            .unwrap();
        assert!(manager.status().kill_all);

        // Also verify EstopManager::load alone establishes the marker when no handle ran first.
        let dir2 = tempdir().unwrap();
        let state_path2 = dir2.path().join("estop-state.json");
        let cfg2 = estop_config(&state_path2);
        fs::write(
            &state_path2,
            serde_json::to_string_pretty(&EstopState::default()).unwrap(),
        )
        .unwrap();
        assert!(!reiclaw_policy::estop::initialized_marker_path(&state_path2).exists());
        let _mgr2 = EstopManager::load(&cfg2, dir2.path()).unwrap();
        assert!(reiclaw_policy::estop::initialized_marker_path(&state_path2).exists());
        fs::remove_file(&state_path2).unwrap();
        let mut mgr2_after = EstopManager::load(&cfg2, dir2.path()).unwrap();
        assert!(mgr2_after.status().kill_all);
        mgr2_after
            .engage(EstopLevel::ToolFreeze(vec!["shell".into()]))
            .unwrap();
        assert!(mgr2_after.status().kill_all);
    }
}
