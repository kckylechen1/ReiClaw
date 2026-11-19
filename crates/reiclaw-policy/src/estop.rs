//! Emergency stop (estop) shared state, strict parser, and live read-only handle.
//!
//! `EstopHandle` is process-shared per canonical state-file path and checks
//! filesystem metadata (`stat`) on every read without holding open file locks or
//! writing back to disk. Any corruption, permission failure, invalid pattern,
//! or disappearance of a previously observed state file fails closed (`kill_all`).

use anyhow::{Context, Result};
use parking_lot::Mutex;
use reiclaw_config::domain_matcher::DomainMatcher;
use reiclaw_config::schema::EstopConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Persisted emergency-stop state (`estop-state.json`).
///
/// Strict deserialization (`deny_unknown_fields`, required core flags/lists)
/// guarantees `{}` or typos fail closed instead of silently unlocking.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EstopState {
    pub kill_all: bool,
    pub network_kill: bool,
    pub blocked_domains: Vec<String>,
    pub frozen_tools: Vec<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl EstopState {
    /// Fail-closed state entered on read/parse/validation errors.
    pub fn fail_closed() -> Self {
        Self {
            kill_all: true,
            network_kill: false,
            blocked_domains: Vec::new(),
            frozen_tools: Vec::new(),
            updated_at: Some(now_rfc3339()),
        }
    }

    pub fn is_engaged(&self) -> bool {
        self.kill_all
            || self.network_kill
            || !self.blocked_domains.is_empty()
            || !self.frozen_tools.is_empty()
    }

    pub fn normalize(&mut self) {
        self.blocked_domains = self
            .blocked_domains
            .iter()
            .map(|domain| domain.trim().to_ascii_lowercase())
            .filter(|domain| !domain.is_empty())
            .collect();
        self.blocked_domains.sort_unstable();
        self.blocked_domains.dedup();

        self.frozen_tools = self
            .frozen_tools
            .iter()
            .map(|tool| tool.trim().to_ascii_lowercase())
            .filter(|tool| !tool.is_empty())
            .collect();
        self.frozen_tools.sort_unstable();
        self.frozen_tools.dedup();
    }
}

/// Resolve `[security.estop] state_file` relative to `config_dir` with `~` expansion.
pub fn resolve_state_file_path(config_dir: &Path, state_file: &str) -> PathBuf {
    let path = crate::expand_user_path(state_file.trim());
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

/// Companion marker file indicating an estop state file was previously initialized.
pub fn initialized_marker_path(state_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.initialized", state_path.display()))
}

/// Return `Ok(true)` if the estop `.initialized` marker exists as a regular non-symlink file.
pub fn has_initialized_marker(state_path: &Path) -> Result<bool> {
    let marker_path = initialized_marker_path(state_path);
    match fs::symlink_metadata(&marker_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                anyhow::bail!(
                    "estop initialized marker {} is not a regular file",
                    marker_path.display()
                );
            }
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to stat estop initialized marker {}",
                marker_path.display()
            )
        }),
    }
}

static MARKER_NONCE: AtomicU64 = AtomicU64::new(0);

/// Atomically create the `0600` `.initialized` marker next to `state_path` if absent.
pub fn ensure_initialized_marker(state_path: &Path) -> Result<()> {
    let marker_path = initialized_marker_path(state_path);
    match fs::symlink_metadata(&marker_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                anyhow::bail!(
                    "estop initialized marker {} is not a regular file",
                    marker_path.display()
                );
            }
            return Ok(());
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to stat estop initialized marker {}",
                    marker_path.display()
                )
            });
        }
    }

    let seq = MARKER_NONCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = PathBuf::from(format!(
        "{}.tmp-{}-{}",
        marker_path.display(),
        std::process::id(),
        seq
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .with_context(|| {
            format!(
                "failed to create temporary estop initialized marker {}",
                temp_path.display()
            )
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to set permissions on estop initialized marker {}",
                    temp_path.display()
                )
            })?;
    }
    drop(file);
    fs::rename(&temp_path, &marker_path).with_context(|| {
        format!(
            "failed to persist estop initialized marker {}",
            marker_path.display()
        )
    })?;
    Ok(())
}

/// Validate and normalize a tool name (including `node:<device>:<cap>`).
pub fn normalize_tool_name(raw: &str) -> Result<String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        anyhow::bail!("Tool name must not be empty");
    }
    if value.starts_with('.') || value.ends_with('.') || value.contains("..") {
        anyhow::bail!("Tool name '{raw}' contains invalid dot placement");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '.'))
    {
        anyhow::bail!("Tool name '{raw}' contains invalid characters");
    }
    Ok(value)
}

pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let timestamp = i64::try_from(secs)
        .ok()
        .and_then(|secs| jiff::Timestamp::from_second(secs).ok())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    reiclaw_api::time::format_rfc3339(timestamp)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    dev: u64,
    ino: u64,
    mode: u32,
    len: u64,
    modified: Option<SystemTime>,
    ctime: i64,
    ctime_nsec: i64,
}

impl FileFingerprint {
    fn from_metadata(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let (dev, ino, mode, ctime, ctime_nsec) = {
            use std::os::unix::fs::MetadataExt;
            (
                meta.dev(),
                meta.ino(),
                meta.mode(),
                meta.ctime(),
                meta.ctime_nsec(),
            )
        };
        #[cfg(not(unix))]
        let (dev, ino, mode, ctime, ctime_nsec) = (0, 0, 0, 0, 0);

        Self {
            dev,
            ino,
            mode,
            len: meta.len(),
            modified: meta.modified().ok(),
            ctime,
            ctime_nsec,
        }
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
}

fn validate_raw_state(state: &EstopState) -> Result<()> {
    for tool in &state.frozen_tools {
        normalize_tool_name(tool)?;
    }
    for domain in &state.blocked_domains {
        let trimmed = domain.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            anyhow::bail!("blocked_domains must not contain empty entries");
        }
        DomainMatcher::validate_pattern(&trimmed)?;
    }
    Ok(())
}

/// Immutable, pre-compiled snapshot of the estop state for hot-path checks.
#[derive(Debug, Clone)]
pub struct EstopSnapshot {
    pub version: u64,
    pub state: EstopState,
    blocked_matcher: Option<DomainMatcher>,
    pub fail_closed_reason: Option<String>,
}

impl EstopSnapshot {
    fn from_valid_state(version: u64, mut state: EstopState) -> Self {
        if let Err(err) = validate_raw_state(&state) {
            return Self::fail_closed(version, format!("invalid estop state entry: {err}"));
        }
        state.normalize();
        let blocked_matcher = if state.blocked_domains.is_empty() {
            None
        } else {
            match DomainMatcher::new(&state.blocked_domains, &[]) {
                Ok(matcher) => Some(matcher),
                Err(err) => {
                    return Self::fail_closed(
                        version,
                        format!("invalid blocked_domains entry in estop state: {err}"),
                    );
                }
            }
        };
        Self {
            version,
            state,
            blocked_matcher,
            fail_closed_reason: None,
        }
    }

    fn fail_closed(version: u64, reason: String) -> Self {
        Self {
            version,
            state: EstopState::fail_closed(),
            blocked_matcher: None,
            fail_closed_reason: Some(reason),
        }
    }

    pub fn is_tool_frozen(&self, tool_name: &str) -> bool {
        let candidate = tool_name.trim();
        self.state
            .frozen_tools
            .iter()
            .any(|frozen| frozen.eq_ignore_ascii_case(candidate))
    }

    pub fn is_domain_blocked(&self, host: &str) -> bool {
        self.blocked_matcher
            .as_ref()
            .is_some_and(|matcher| matcher.is_gated(host))
    }
}

#[derive(Debug)]
struct CachedState {
    fingerprint: Option<FileFingerprint>,
    snapshot: Arc<EstopSnapshot>,
}

/// Process-shared read-only handle to a live `estop-state.json` file.
#[derive(Debug)]
pub struct EstopHandle {
    state_path: PathBuf,
    seen_file: AtomicBool,
    cache: Mutex<CachedState>,
}

static ESTOP_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<EstopHandle>>>> = OnceLock::new();

fn canonical_registry_key(path: &Path) -> PathBuf {
    if let Some(parent) = path.parent()
        && let Some(name) = path.file_name()
        && let Ok(canon_parent) = parent.canonicalize()
    {
        return canon_parent.join(name);
    }
    path.to_path_buf()
}

impl EstopHandle {
    /// Obtain (or share) the process-wide read-only handle for `config` + `config_dir`.
    pub fn for_config(config: &EstopConfig, config_dir: &Path) -> Arc<Self> {
        let resolved = resolve_state_file_path(config_dir, &config.state_file);
        Self::for_path(resolved)
    }

    /// Obtain (or share) the process-wide read-only handle for an explicit `state_path`.
    pub fn for_path(state_path: PathBuf) -> Arc<Self> {
        let key = canonical_registry_key(&state_path);
        let registry = ESTOP_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = registry.lock();
        if let Some(existing) = guard.get(&key) {
            return Arc::clone(existing);
        }
        let handle = Arc::new(Self {
            state_path,
            seen_file: AtomicBool::new(false),
            cache: Mutex::new(CachedState {
                fingerprint: None,
                snapshot: Arc::new(EstopSnapshot::from_valid_state(0, EstopState::default())),
            }),
        });
        guard.insert(key, Arc::clone(&handle));
        handle
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    fn fail_closed_locked(&self, cache: &mut CachedState, reason: String) -> Arc<EstopSnapshot> {
        if cache.snapshot.fail_closed_reason.as_deref() != Some(reason.as_str())
            || cache.fingerprint.is_some()
        {
            let next_version = cache.snapshot.version.saturating_add(1);
            cache.fingerprint = None;
            cache.snapshot = Arc::new(EstopSnapshot::fail_closed(next_version, reason));
        }
        Arc::clone(&cache.snapshot)
    }

    /// Return the latest verified snapshot, refreshing via `lstat()` + single-fd `fstat()`/`read()`
    /// whenever the underlying inode/mode/size/mtime/ctime changes.
    pub fn snapshot(&self) -> Arc<EstopSnapshot> {
        let lstat_meta = match fs::symlink_metadata(&self.state_path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let marker_status = has_initialized_marker(&self.state_path);
                let mut cache = self.cache.lock();
                let initialized_err = match &marker_status {
                    Ok(true) => Some(format!(
                        "estop state file {} missing while initialized marker is present",
                        self.state_path.display()
                    )),
                    Err(marker_err) => Some(format!(
                        "estop state file {} missing and initialized marker check failed: {marker_err}",
                        self.state_path.display()
                    )),
                    Ok(false) => None,
                };
                if let Some(reason) = initialized_err {
                    return self.fail_closed_locked(&mut cache, reason);
                }
                if self.seen_file.load(Ordering::Acquire) {
                    if cache.snapshot.fail_closed_reason.is_none() || cache.fingerprint.is_some() {
                        let next_version = cache.snapshot.version.saturating_add(1);
                        let reason = format!(
                            "estop state file {} disappeared after being observed",
                            self.state_path.display()
                        );
                        ::reiclaw_log::record!(
                            ERROR,
                            ::reiclaw_log::Event::new(
                                module_path!(),
                                ::reiclaw_log::Action::Reject
                            )
                            .with_outcome(::reiclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "path": self.state_path.display().to_string(),
                                "error": &reason,
                            })),
                            "estop state file vanished; failing closed"
                        );
                        cache.fingerprint = None;
                        cache.snapshot = Arc::new(EstopSnapshot::fail_closed(next_version, reason));
                    }
                } else if cache.fingerprint.is_some() || cache.snapshot.state.is_engaged() {
                    let next_version = cache.snapshot.version.saturating_add(1);
                    cache.fingerprint = None;
                    cache.snapshot = Arc::new(EstopSnapshot::from_valid_state(
                        next_version,
                        EstopState::default(),
                    ));
                }
                return Arc::clone(&cache.snapshot);
            }
            Err(err) => {
                let mut cache = self.cache.lock();
                let reason = format!(
                    "failed to stat estop state file {}: {err}",
                    self.state_path.display()
                );
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "path": self.state_path.display().to_string(),
                            "error": err.to_string(),
                        })),
                    "estop state stat failed; failing closed"
                );
                return self.fail_closed_locked(&mut cache, reason);
            }
        };

        self.seen_file.store(true, Ordering::Release);
        if lstat_meta.file_type().is_symlink() || !lstat_meta.file_type().is_file() {
            let mut cache = self.cache.lock();
            let reason = format!(
                "estop state path {} is a symlink or non-regular file",
                self.state_path.display()
            );
            ::reiclaw_log::record!(
                ERROR,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": self.state_path.display().to_string(),
                        "error": &reason,
                    })),
                "estop state path rejected; failing closed"
            );
            return self.fail_closed_locked(&mut cache, reason);
        }

        let lstat_fp = FileFingerprint::from_metadata(&lstat_meta);

        let mut cache = self.cache.lock();
        if cache.fingerprint.as_ref() == Some(&lstat_fp) {
            return Arc::clone(&cache.snapshot);
        }

        let next_version = cache.snapshot.version.saturating_add(1);
        let mut file = match fs::File::open(&self.state_path) {
            Ok(file) => file,
            Err(err) => {
                let reason = format!(
                    "failed to open estop state file {}: {err}",
                    self.state_path.display()
                );
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "path": self.state_path.display().to_string(),
                            "error": err.to_string(),
                        })),
                    "estop state open failed; failing closed"
                );
                return self.fail_closed_locked(&mut cache, reason);
            }
        };

        let fstat_meta = match file.metadata() {
            Ok(meta) => meta,
            Err(err) => {
                let reason = format!(
                    "failed to fstat estop state file {}: {err}",
                    self.state_path.display()
                );
                return self.fail_closed_locked(&mut cache, reason);
            }
        };
        let fstat_fp = FileFingerprint::from_metadata(&fstat_meta);
        if !fstat_meta.file_type().is_file() || !fstat_fp.same_identity(&lstat_fp) {
            let reason = format!(
                "estop state file {} changed identity or is not a regular file",
                self.state_path.display()
            );
            return self.fail_closed_locked(&mut cache, reason);
        }

        let mut raw = String::new();
        let next_snapshot = match file.read_to_string(&mut raw) {
            Ok(_) => match serde_json::from_str::<EstopState>(&raw) {
                Ok(parsed) => {
                    let snap = EstopSnapshot::from_valid_state(next_version, parsed);
                    if snap.fail_closed_reason.is_none()
                        && !matches!(has_initialized_marker(&self.state_path), Ok(true))
                    {
                        // Pre-upgrade estop-state.json may lack the durable marker; backfill it
                        // on first valid observation so subsequent deletion still fails closed
                        // (best-effort on unwritable directories).
                        let _ = ensure_initialized_marker(&self.state_path);
                    }
                    snap
                }
                Err(err) => {
                    let reason = format!(
                        "failed to parse estop state file {}: {err}",
                        self.state_path.display()
                    );
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                            .with_outcome(::reiclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "path": self.state_path.display().to_string(),
                                "error": err.to_string(),
                            })),
                        "estop state parse failed; failing closed"
                    );
                    EstopSnapshot::fail_closed(next_version, reason)
                }
            },
            Err(err) => {
                let reason = format!(
                    "failed to read estop state file {}: {err}",
                    self.state_path.display()
                );
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "path": self.state_path.display().to_string(),
                            "error": err.to_string(),
                        })),
                    "estop state read failed; failing closed"
                );
                return self.fail_closed_locked(&mut cache, reason);
            }
        };

        cache.fingerprint = Some(fstat_fp);
        cache.snapshot = Arc::new(next_snapshot);
        Arc::clone(&cache.snapshot)
    }

    pub fn version(&self) -> u64 {
        self.snapshot().version
    }

    pub fn is_kill_all(&self) -> bool {
        self.snapshot().state.kill_all
    }

    pub fn is_network_killed(&self) -> bool {
        let snap = self.snapshot();
        snap.state.kill_all || snap.state.network_kill
    }

    pub fn enforce_not_killed(&self) -> std::result::Result<(), String> {
        let snap = self.snapshot();
        if snap.state.kill_all {
            if let Some(reason) = &snap.fail_closed_reason {
                return Err(format!(
                    "blocked by security policy: emergency stop fail-closed (kill_all): {reason}"
                ));
            }
            return Err("blocked by security policy: emergency stop (kill_all) is active".into());
        }
        Ok(())
    }

    pub fn enforce_tool(&self, tool_name: &str) -> std::result::Result<(), String> {
        let snap = self.snapshot();
        if snap.state.kill_all {
            if let Some(reason) = &snap.fail_closed_reason {
                return Err(format!(
                    "blocked by security policy: emergency stop fail-closed (kill_all): {reason}"
                ));
            }
            return Err("blocked by security policy: emergency stop (kill_all) is active".into());
        }
        if snap.is_tool_frozen(tool_name) {
            return Err(format!(
                "blocked by security policy: tool '{tool_name}' is frozen by emergency stop"
            ));
        }
        Ok(())
    }

    pub fn enforce_network(&self, host: Option<&str>) -> std::result::Result<(), String> {
        let snap = self.snapshot();
        if snap.state.kill_all {
            if let Some(reason) = &snap.fail_closed_reason {
                return Err(format!(
                    "blocked by security policy: emergency stop fail-closed (kill_all): {reason}"
                ));
            }
            return Err("blocked by security policy: emergency stop (kill_all) is active".into());
        }
        if snap.state.network_kill {
            return Err(
                "blocked by security policy: emergency stop (network_kill) is active".into(),
            );
        }
        if let Some(host) = host
            && snap.is_domain_blocked(host)
        {
            return Err(format!(
                "blocked by security policy: domain '{host}' is blocked by emergency stop"
            ));
        }
        Ok(())
    }
}

/// Strictly parse raw JSON into `EstopState` (for `EstopManager` & tests).
pub fn parse_estop_state_strict(raw: &str) -> Result<EstopState> {
    let mut parsed: EstopState =
        serde_json::from_str(raw).context("Invalid estop state JSON payload")?;
    validate_raw_state(&parsed)?;
    parsed.normalize();
    if !parsed.blocked_domains.is_empty() {
        DomainMatcher::new(&parsed.blocked_domains, &[])?;
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_atomic(path: &Path, body: &str) {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, body).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn empty_json_object_and_unknown_fields_fail_closed() {
        assert!(parse_estop_state_strict("{}").is_err());
        assert!(
            parse_estop_state_strict(
                r#"{"kil_all":true,"network_kill":false,"blocked_domains":[],"frozen_tools":[]}"#
            )
            .is_err()
        );

        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-empty.json");
        fs::write(&path, "{}").unwrap();
        let handle = EstopHandle::for_path(path);
        assert!(handle.enforce_not_killed().is_err());
        assert!(handle.is_kill_all());
    }

    #[test]
    fn missing_before_seen_is_open_but_disappearing_after_seen_fails_closed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-vanish.json");
        let handle = EstopHandle::for_path(path.clone());
        assert!(handle.enforce_not_killed().is_ok());

        let valid = serde_json::to_string(&EstopState::default()).unwrap();
        write_atomic(&path, &valid);
        assert!(handle.enforce_not_killed().is_ok());

        fs::remove_file(&path).unwrap();
        let err = handle.enforce_not_killed().unwrap_err();
        assert!(err.contains("fail-closed"));
    }

    #[test]
    fn atomic_rename_updates_live_handle_without_restart() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-live.json");
        let handle = EstopHandle::for_path(path.clone());
        assert!(handle.enforce_tool("node:phone:system.info").is_ok());

        let engaged = EstopState {
            kill_all: false,
            network_kill: false,
            blocked_domains: vec!["*.chase.com".into()],
            frozen_tools: vec!["node:phone:system.info".into()],
            updated_at: Some(now_rfc3339()),
        };
        write_atomic(&path, &serde_json::to_string(&engaged).unwrap());

        assert!(handle.enforce_tool("node:phone:system.info").is_err());
        assert!(handle.enforce_tool("NODE:PHONE:SYSTEM.INFO").is_err());
        assert!(handle.enforce_tool("file_read").is_ok());
        assert!(handle.enforce_network(Some("secure.chase.com")).is_err());
        assert!(handle.enforce_network(Some("example.com")).is_ok());

        let resumed = EstopState::default();
        write_atomic(&path, &serde_json::to_string(&resumed).unwrap());
        assert!(handle.enforce_tool("node:phone:system.info").is_ok());
        assert!(handle.enforce_network(Some("secure.chase.com")).is_ok());
    }

    #[test]
    fn invalid_domain_pattern_in_state_file_fails_closed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-bad-domain.json");
        let bad = EstopState {
            kill_all: false,
            network_kill: false,
            blocked_domains: vec!["bad domain.com".into()],
            frozen_tools: Vec::new(),
            updated_at: None,
        };
        write_atomic(&path, &serde_json::to_string(&bad).unwrap());
        let handle = EstopHandle::for_path(path);
        assert!(handle.enforce_not_killed().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_state_file_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-unreadable.json");
        fs::write(
            &path,
            serde_json::to_string(&EstopState::default()).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores chmod 000; skip when the OS still permits reading it.
        if fs::read_to_string(&path).is_ok() {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            return;
        }

        let handle = EstopHandle::for_path(path.clone());
        assert!(handle.enforce_not_killed().is_err());

        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }

    #[cfg(unix)]
    #[test]
    fn post_cache_chmod_zero_busts_cache_and_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-post-cache-chmod.json");
        write_atomic(
            &path,
            &serde_json::to_string(&EstopState::default()).unwrap(),
        );

        let handle = EstopHandle::for_path(path.clone());
        assert!(handle.enforce_not_killed().is_ok());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_to_string(&path).is_ok() {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            return;
        }

        assert!(handle.enforce_not_killed().is_err());
        assert!(handle.is_kill_all());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(handle.enforce_not_killed().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_fails_closed_on_first_read() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-dangling-link.json");
        symlink(dir.path().join("missing-target.json"), &path).unwrap();

        let handle = EstopHandle::for_path(path);
        assert!(handle.enforce_not_killed().is_err());
        assert!(handle.is_kill_all());
    }

    #[test]
    fn blank_entries_fail_closed_before_normalization() {
        let blank_tool_json = r#"{"kill_all":false,"network_kill":false,"blocked_domains":[],"frozen_tools":["   "]}"#;
        assert!(parse_estop_state_strict(blank_tool_json).is_err());

        let blank_domain_json =
            r#"{"kill_all":false,"network_kill":false,"blocked_domains":["  "],"frozen_tools":[]}"#;
        assert!(parse_estop_state_strict(blank_domain_json).is_err());

        let dir = tempdir().unwrap();
        let path = dir.path().join("estop-blank-entries.json");
        write_atomic(&path, blank_tool_json);
        let handle = EstopHandle::for_path(path.clone());
        assert!(handle.enforce_not_killed().is_err());
        assert!(handle.is_kill_all());

        write_atomic(&path, blank_domain_json);
        assert!(handle.enforce_not_killed().is_err());
        assert!(handle.is_kill_all());
    }
}
