//! Configuration loading, bootstrap, and install-directory resolution.
//!
//! The schema owns configuration shapes. This module retains the existing
//! loading order, filesystem effects, warnings, and environment precedence.

use crate::save::collect_onepassword_reference_snapshots;
use crate::schema::{
    Config, EnvOverriddenPaths, ExternalRefSnapshots, PreOverrideSnapshots, ProxyConfig,
};
use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::fs;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeConfigKind {
    Default,
    Custom,
    Temporary,
}

/// Read the persisted `[proxy]` section using the same install-directory
/// resolution as [`Config::load_or_init`], for the pre-runtime startup
/// application of proxy state. Returns `None` when the install or its
/// config file cannot be read (including a fresh install with no file
/// yet), or when the on-disk schema version is not the current one: a
/// missing, older or future version is rejected by the full loader and must
/// not change process-wide proxy state. Proxy fields are never secret-annotated,
/// so plain parsing is sufficient.
///
/// This is a raw file read by design: `REI_PROXY_*` env-var
/// overrides are NOT reflected here. The runtime global is refreshed
/// from the fully-loaded config after `load_or_init` runs; the process
/// env stays restart-only and follows the persisted file.
pub async fn read_persisted_proxy_for_boot() -> Option<ProxyConfig> {
    let (default_rei_dir, default_workspace_dir) = default_config_and_data_dirs().ok()?;
    let (rei_dir, _legacy_workspace_dir, _source) =
        resolve_runtime_config_dirs(&default_rei_dir, &default_workspace_dir)
            .await
            .ok()?;
    let raw = std::fs::read_to_string(rei_dir.join("config.toml")).ok()?;
    let value: toml::Value = toml::from_str(&raw).ok()?;
    if crate::current::detect_version(&value).ok() != Some(crate::current::CURRENT_SCHEMA_VERSION) {
        return None;
    }
    let proxy_table = value.get("proxy")?.clone();
    ProxyConfig::deserialize(proxy_table).ok()
}

pub(crate) fn default_config_and_data_dirs() -> Result<(PathBuf, PathBuf)> {
    let config_dir = default_config_dir()?;
    // The second value is the shared instance data directory
    // (databases + state files). Per-agent identity + markdown lives
    // at `<config-dir>/agents/<alias>/workspace/`, resolved separately
    // via `Config::agent_workspace_dir`.
    Ok((config_dir.clone(), config_dir.join("data")))
}

/// Ordered config-directory override env vars, highest precedence first.
const CONFIG_DIR_ENV_VARS: &[&str] = &["REI_CONFIG_DIR", "REICLAW_CONFIG_DIR"];

pub(crate) fn env_config_dir_override() -> Option<(&'static str, String)> {
    for &name in CONFIG_DIR_ENV_VARS {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some((name, trimmed.to_owned()));
            }
        }
    }
    None
}

pub(crate) fn resolve_home_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home));
    }

    UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

fn default_home_config_dir(home: &Path) -> PathBuf {
    home.join(".rei")
}

fn default_config_dir() -> Result<PathBuf> {
    if let Some((_name, custom)) = env_config_dir_override() {
        return Ok(expand_tilde_path(&custom));
    }

    let home = resolve_home_dir().context("Could not find home directory")?;
    Ok(default_home_config_dir(&home))
}

/// Canonical on-disk directory for a locale's runtime FTL catalogues:
/// `<config_dir>/data/ftl/<locale>/`. This is where `rei locales fetch`
/// restores bundled translations and where the runtime i18n loader reads them.
/// `<config_dir>` honors `REI_CONFIG_DIR` and `REICLAW_CONFIG_DIR`, and
/// otherwise defaults to `~/.rei`.
pub fn ftl_locale_dir(locale: &str) -> Result<PathBuf> {
    Ok(default_config_dir()?.join("data").join("ftl").join(locale))
}

/// The FTL catalogues that `rei locales fetch` can restore from the
/// current build, as `(name, output-filename)`. This is the single
/// source of truth — a caller supplies only a catalog *name* matched against
/// this table, never a path.
pub const FTL_CATALOGS: &[(&str, &str)] = &[("cli", "cli.ftl"), ("tools", "tools.ftl")];

/// Build a default path string by joining `relative` onto the resolved
/// platform config dir. The form sees the resolved absolute path
/// (`/home/<user>/.rei/<relative>` on Linux,
/// `C:\Users\<user>\.rei\<relative>` on Windows, etc.) instead of a
/// literal `~/...` token that doesn't expand on Windows. Falls back to
/// `~/.rei/<relative>` if the platform dir can't be resolved (rare —
/// e.g. no HOME and `directories::UserDirs` returns None); the runtime's
/// `expand_tilde_path()` handles that literal at use-time.
///
/// Switching to platform-native config locations (`~/Library/Application
/// Support/reiclaw/` on macOS, `%APPDATA%\reiclaw\` on Windows) is the
/// schema-v3 follow-up tracked in — that needs a migration to move
/// existing users' configs.
pub(crate) fn default_path_under_config_dir(relative: &str) -> String {
    match default_config_dir() {
        Ok(dir) => dir.join(relative).to_string_lossy().into_owned(),
        Err(_) => format!("~/.rei/{relative}"),
    }
}

pub fn resolve_config_dir_for_data(data_dir: &Path) -> (PathBuf, PathBuf) {
    let data_config_dir = data_dir.to_path_buf();
    if data_config_dir.join("config.toml").exists() {
        return (data_config_dir.clone(), data_config_dir.join("data"));
    }

    let legacy_config_dir = data_dir.parent().map(|parent| parent.join(".rei"));
    if let Some(legacy_dir) = legacy_config_dir {
        if legacy_dir.join("config.toml").exists() {
            return (legacy_dir, data_config_dir);
        }

        // Retain both supported environment path spellings. This only
        // resolves locations; it does not convert configuration or move data.
        if data_dir.file_name().is_some_and(|name| {
            name == std::ffi::OsStr::new("data") || name == std::ffi::OsStr::new("workspace")
        }) {
            return (legacy_dir, data_config_dir);
        }
    }

    (data_config_dir.clone(), data_config_dir.join("data"))
}

pub async fn classify_runtime_config_kind(config_path: &Path) -> RuntimeConfigKind {
    if path_is_under(config_path, &std::env::temp_dir()) {
        return RuntimeConfigKind::Temporary;
    }

    let Ok((default_config_dir, default_data_dir)) = default_config_and_data_dirs() else {
        return RuntimeConfigKind::Custom;
    };
    let Ok((resolved_config_dir, _data_dir, source)) =
        resolve_runtime_config_dirs(&default_config_dir, &default_data_dir).await
    else {
        return RuntimeConfigKind::Custom;
    };

    match source {
        ConfigResolutionSource::DefaultConfigDir => {
            let matches_resolved =
                paths_equal_lexically(config_path, &resolved_config_dir.join("config.toml"));
            let matches_known_home_default = resolve_home_dir().is_some_and(|home| {
                paths_equal_lexically(config_path, &home.join(".rei").join("config.toml"))
            });
            if matches_resolved || matches_known_home_default {
                RuntimeConfigKind::Default
            } else {
                RuntimeConfigKind::Custom
            }
        }
        ConfigResolutionSource::HomebrewConfigDir => {
            if paths_equal_lexically(config_path, &resolved_config_dir.join("config.toml")) {
                RuntimeConfigKind::Default
            } else {
                RuntimeConfigKind::Custom
            }
        }
        ConfigResolutionSource::EnvConfigDir
        | ConfigResolutionSource::EnvDataDir
        | ConfigResolutionSource::EnvWorkspaceLegacy => RuntimeConfigKind::Custom,
    }
}

/// Resolve the current runtime config/data directories.
///
/// This mirrors the same precedence used by `Config::load_or_init()`:
/// `REI_CONFIG_DIR` > `REICLAW_CONFIG_DIR` >
/// `REI_DATA_DIR` > `REI_WORKSPACE` (deprecated) > defaults.
pub async fn resolve_runtime_dirs() -> Result<(PathBuf, PathBuf)> {
    let (default_rei_dir, default_data_dir) = default_config_and_data_dirs()?;
    let (config_dir, data_dir, _) =
        resolve_runtime_config_dirs(&default_rei_dir, &default_data_dir).await?;
    Ok((config_dir, data_dir))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigResolutionSource {
    EnvConfigDir,
    EnvDataDir,
    EnvWorkspaceLegacy,
    DefaultConfigDir,
    HomebrewConfigDir,
}

impl ConfigResolutionSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::EnvConfigDir => "REI_CONFIG_DIR",
            Self::EnvDataDir => "REI_DATA_DIR",
            Self::EnvWorkspaceLegacy => "REI_WORKSPACE",
            Self::DefaultConfigDir => "default",
            Self::HomebrewConfigDir => "homebrew",
        }
    }
}

/// Expand tilde in paths, falling back to `UserDirs` when HOME is unset.
///
/// In non-TTY environments (e.g. cron), HOME may not be set, causing
/// `shellexpand::tilde` to return the literal `~` unexpanded. This helper
/// detects that case and uses `directories::UserDirs` as a fallback.
pub(crate) fn expand_tilde_path(path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let expanded_str = expanded.as_ref();

    // If the path still starts with '~', tilde expansion failed (HOME unset)
    if expanded_str.starts_with('~') {
        if let Some(user_dirs) = UserDirs::new() {
            let home = user_dirs.home_dir();
            // Replace leading ~ with home directory
            if let Some(rest) = expanded_str.strip_prefix('~') {
                return home.join(rest.trim_start_matches(['/', '\\']));
            }
        }
        // If UserDirs also fails, log a warning and use the literal path
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(
                crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                ::reiclaw_log::Action::Note
            )
            .with_outcome(::reiclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({"path": path})),
            "Failed to expand tilde: HOME environment variable is not set and UserDirs failed. \
             In cron/non-TTY environments, use absolute paths or set HOME explicitly."
        );
    }

    PathBuf::from(expanded_str)
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    path.components()
        .zip(root.components())
        .all(|(a, b)| a == b)
        && path.components().count() >= root.components().count()
}

fn paths_equal_lexically(a: &Path, b: &Path) -> bool {
    a.components().eq(b.components())
}

/// Detect if an executable path lives under a macOS Homebrew prefix and return
/// the Homebrew-managed config directory.
///
/// Homebrew can execute ReiClaw from `<prefix>/Cellar/rei/<version>/bin/`,
/// `<prefix>/bin/`, or `<prefix>/opt/rei/bin/`.
pub(crate) async fn try_resolve_macos_homebrew_config_dir(exe: &Path) -> Option<PathBuf> {
    let parts = exe.iter().collect::<Vec<_>>();
    let prefix = match parts.as_slice() {
        [prefix @ .., cellar, formula, _version, bin, exe_name]
            if *cellar == std::ffi::OsStr::new("Cellar")
                && *formula == std::ffi::OsStr::new("rei")
                && *bin == std::ffi::OsStr::new("bin")
                && *exe_name == std::ffi::OsStr::new("rei") =>
        {
            prefix.iter().collect::<PathBuf>()
        }
        [prefix @ .., opt, formula, bin, exe_name]
            if *opt == std::ffi::OsStr::new("opt")
                && *formula == std::ffi::OsStr::new("rei")
                && *bin == std::ffi::OsStr::new("bin")
                && *exe_name == std::ffi::OsStr::new("rei") =>
        {
            let prefix = prefix.iter().collect::<PathBuf>();
            if !prefix.as_os_str().is_empty()
                && fs::metadata(prefix.join("Cellar"))
                    .await
                    .is_ok_and(|metadata| metadata.is_dir())
            {
                prefix
            } else {
                return None;
            }
        }
        [prefix @ .., bin, exe_name]
            if *bin == std::ffi::OsStr::new("bin") && *exe_name == std::ffi::OsStr::new("rei") =>
        {
            let prefix = prefix.iter().collect::<PathBuf>();
            if !prefix.as_os_str().is_empty()
                && fs::metadata(prefix.join("Cellar"))
                    .await
                    .is_ok_and(|metadata| metadata.is_dir())
            {
                prefix
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(prefix.join("var").join("rei"))
}

pub(crate) async fn resolve_runtime_config_dirs(
    default_rei_dir: &Path,
    default_data_dir: &Path,
) -> Result<(PathBuf, PathBuf, ConfigResolutionSource)> {
    if let Some((env_name, custom_config_dir)) = env_config_dir_override() {
        // If the operator ALSO set REI_DATA_DIR or
        // REI_WORKSPACE, CONFIG_DIR wins; surface the
        // collision so they know which one took effect.
        if std::env::var("REI_DATA_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some()
        {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::reiclaw_log::Action::Note
                )
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                &format!(
                    "{env_name} is set; REI_DATA_DIR is ignored \
                     (CONFIG_DIR pins both the config directory and the data \
                     directory under it)."
                )
            );
        }
        if std::env::var("REI_WORKSPACE")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::reiclaw_log::Action::Note
                )
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                &format!(
                    "{env_name} is set; REI_WORKSPACE (deprecated) \
                     is ignored. REI_WORKSPACE will be removed in a future \
                     release; switch any remaining references to REI_DATA_DIR."
                )
            );
        }
        let rei_dir = expand_tilde_path(&custom_config_dir);
        return Ok((
            rei_dir.clone(),
            rei_dir.join("data"),
            ConfigResolutionSource::EnvConfigDir,
        ));
    }

    if let Ok(custom_data) = std::env::var("REI_DATA_DIR")
        && !custom_data.trim().is_empty()
    {
        if std::env::var("REI_WORKSPACE")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::reiclaw_log::Action::Note
                )
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                "REI_DATA_DIR and REI_WORKSPACE are both set; \
                 REI_WORKSPACE (deprecated) is ignored. \
                 REI_WORKSPACE will be removed in a future release."
            );
        }
        let expanded = expand_tilde_path(&custom_data);
        let (rei_dir, data_dir) = resolve_config_dir_for_data(&expanded);
        return Ok((rei_dir, data_dir, ConfigResolutionSource::EnvDataDir));
    }

    if let Ok(custom_workspace) = std::env::var("REI_WORKSPACE")
        && !custom_workspace.is_empty()
    {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(
                crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                ::reiclaw_log::Action::Note
            )
            .with_outcome(::reiclaw_log::EventOutcome::Unknown),
            "REI_WORKSPACE is deprecated; use REI_DATA_DIR instead. \
             REI_WORKSPACE will be removed in a future release."
        );
        let expanded = expand_tilde_path(&custom_workspace);
        let (rei_dir, data_dir) = resolve_config_dir_for_data(&expanded);
        return Ok((
            rei_dir,
            data_dir,
            ConfigResolutionSource::EnvWorkspaceLegacy,
        ));
    }

    if cfg!(target_os = "macos")
        && let Ok(exe) = std::env::current_exe()
        && let Some(homebrew_config_dir) = try_resolve_macos_homebrew_config_dir(&exe).await
    {
        return Ok((
            homebrew_config_dir.clone(),
            homebrew_config_dir.join("workspace"),
            ConfigResolutionSource::HomebrewConfigDir,
        ));
    }

    Ok((
        default_rei_dir.to_path_buf(),
        default_data_dir.to_path_buf(),
        ConfigResolutionSource::DefaultConfigDir,
    ))
}

pub(crate) fn config_dir_creation_error(path: &Path) -> String {
    format!(
        "Failed to create config directory: {}. If running as an OpenRC service, \
         ensure this path is writable by user 'rei'.",
        path.display()
    )
}

/// Ensure that essential bootstrap files exist in the workspace directory.
///
/// When the workspace is created outside of Quickstart (e.g., non-tty
/// daemon/cron sessions), these files would otherwise be missing. This function
/// creates sensible defaults that allow the agent to operate with a basic identity.
pub async fn ensure_bootstrap_files(workspace_dir: &Path) -> Result<()> {
    let defaults: &[(&str, &str)] = &[
        (
            "IDENTITY.md",
            "# IDENTITY.md — Who Am I?\n\n\
             I am ReiClaw, an autonomous AI agent.\n\n\
             ## Traits\n\
             - Helpful, precise, and safety-conscious\n\
             - I prioritize clarity and correctness\n",
        ),
        (
            "SOUL.md",
            "# SOUL.md — Who You Are\n\n\
             You are ReiClaw, an autonomous AI agent.\n\n\
             ## Core Principles\n\
             - Be helpful and accurate\n\
             - Respect user intent and boundaries\n\
             - Ask before taking destructive actions\n\
             - Prefer safe, reversible operations\n",
        ),
    ];

    for (filename, content) in defaults {
        let path = workspace_dir.join(filename);
        if !path.exists() {
            fs::write(&path, content)
                .await
                .with_context(|| format!("Failed to create default {filename} in workspace"))?;
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExtraNestedModelProviderTable {
    pub(crate) family: String,
    pub(crate) alias: String,
    pub(crate) nested: String,
}

impl Config {
    /// Return top-level TOML keys in `raw_toml` that Config does not recognise.
    ///
    /// Keys present in `Config::default()` serialization pass immediately.
    /// Remaining keys are probed: the key is deserialized in isolation and
    /// the result compared to the default — a changed output means serde
    /// consumed it (covers `Option<T>` fields and `#[serde(alias)]` names).
    pub fn unknown_keys(raw_toml: &str) -> Vec<String> {
        let raw: toml::Table = match raw_toml.parse() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        static DEFAULTS: OnceLock<toml::Table> = OnceLock::new();
        let defaults = DEFAULTS.get_or_init(|| {
            toml::to_string(&Config::default())
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default()
        });
        raw.keys()
            .filter(|key| {
                if defaults.contains_key(key.as_str()) {
                    return false;
                }
                let mut t = toml::Table::new();
                t.insert((*key).clone(), raw[key.as_str()].clone());
                let consumed = toml::to_string(&t)
                    .ok()
                    .and_then(|s| toml::from_str::<Config>(&s).ok())
                    .and_then(|c| toml::to_string(&c).ok())
                    .and_then(|s| s.parse::<toml::Table>().ok())
                    .is_some_and(|t| t != *defaults);
                !consumed
            })
            .cloned()
            .collect()
    }

    /// Return `<kind>.<family>` entries under `[providers]` in `raw_toml`
    /// whose family is not a known typed slot (kinds: models, tts,
    /// transcription). Serde silently drops these sections at deserialize
    /// time, so a typo'd family (`[providers.models.antropic.x]`) or one
    /// from a newer binary vanishes on reload: the alias "works" in the
    /// session that created it, then disappears after restart.
    pub fn unknown_provider_families(raw_toml: &str) -> Vec<String> {
        let raw: toml::Table = match raw_toml.parse() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let Some(kinds) = raw.get("providers").and_then(toml::Value::as_table) else {
            return Vec::new();
        };
        let kind_slots: &[(&str, &[&str])] = &[
            ("models", crate::providers::ModelProviders::slot_names()),
            ("tts", crate::providers::TtsProviders::slot_names()),
            (
                "transcription",
                crate::providers::TranscriptionProviders::slot_names(),
            ),
        ];
        let mut out = Vec::new();
        for (kind, slots) in kind_slots {
            let Some(families) = kinds.get(*kind).and_then(toml::Value::as_table) else {
                continue;
            };
            out.extend(
                families
                    .keys()
                    .filter(|k| !slots.contains(&k.as_str()))
                    .map(|k| format!("{kind}.{k}")),
            );
        }
        out
    }

    /// Return extra-nested model-provider alias tables that look like a
    /// misplaced provider config, e.g.
    /// `[providers.models.zai.default.default]`.
    ///
    /// Serde treats the first `default` as the alias and silently ignores the
    /// second `default` child table. When that child table carries provider
    /// fields such as `model`, `api_key`, or family-specific fields, the
    /// operator's settings are dropped while the empty alias still validates as
    /// an in-progress provider. This raw-TOML detector runs before that shape
    /// is erased by typed deserialization.
    pub(crate) fn extra_nested_model_provider_tables(
        raw_toml: &str,
    ) -> Vec<ExtraNestedModelProviderTable> {
        let raw: toml::Table = match raw_toml.parse() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let Some(models) = raw
            .get("providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("models"))
            .and_then(toml::Value::as_table)
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for (family, aliases_value) in models {
            if !crate::providers::ModelProviders::slot_names().contains(&family.as_str()) {
                continue;
            }
            let Some(aliases) = aliases_value.as_table() else {
                continue;
            };
            for (alias, alias_value) in aliases {
                let Some(alias_table) = alias_value.as_table() else {
                    continue;
                };
                for (nested_key, nested_value) in alias_table {
                    let Some(nested_table) = nested_value.as_table() else {
                        continue;
                    };
                    if nested_table.is_empty()
                        || Self::is_model_provider_table_valued_field(nested_key)
                    {
                        continue;
                    }
                    out.push(ExtraNestedModelProviderTable {
                        family: family.clone(),
                        alias: alias.clone(),
                        nested: nested_key.clone(),
                    });
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn is_model_provider_table_valued_field(key: &str) -> bool {
        // These shared model-provider fields legitimately deserialize from
        // nested TOML tables under an alias. Every other table-valued child of
        // `[providers.models.<family>.<alias>]` is an ignored extra section,
        // regardless of whether its contents are shared or family-specific
        // provider fields.
        matches!(
            key,
            "chat_template_kwargs" | "extra_headers" | "pricing" | "provider_extra"
        )
    }

    pub async fn load_or_init() -> Result<Self> {
        let (default_rei_dir, default_workspace_dir) = default_config_and_data_dirs()?;

        // Resolve the operator's install root before inspecting its config.
        let (rei_dir, _legacy_workspace_dir, resolution_source) =
            resolve_runtime_config_dirs(&default_rei_dir, &default_workspace_dir).await?;
        let config_path = rei_dir.join("config.toml");
        let persisted_contents = match fs::read_to_string(&config_path).await {
            Ok(contents) => Some(contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("Failed to read config file"),
        };
        // An unsupported generation is a hard refusal before any install-layout
        // changes. Malformed TOML retains the explicitly degraded repair path.
        if let Some(contents) = persisted_contents.as_deref()
            && let Ok(raw) = toml::from_str::<toml::Value>(contents)
        {
            crate::current::require_current_schema(&raw)?;
        }
        if let Err(err) = crate::install::relocate_default_agent_skills_to_shared(&rei_dir) {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::reiclaw_log::Action::Note
                )
                .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "install": rei_dir.display().to_string(),
                    "error": format!("{err}"),
                })),
                "[system] skills relocation to shared workspace failed; continuing"
            );
        }

        // The install dir is the only directory `load_or_init` creates
        // unconditionally. Per-agent workspaces (`agents/<alias>/workspace/`)
        // are seeded lazily at agent-loop entry by
        // `Agent::from_config_with_session_cwd_and_mcp`, which runs
        // `ensure_bootstrap_files` for the agent it is starting. There
        // is no implicit "default" agent or synthesized workspace at boot.
        //
        // `config.data_dir` resolves to `<install>/data/` — the shared
        // instance data directory holding databases (memory, sessions,
        // cost records) and hygiene/state files. Per-agent identity
        // and markdown (MEMORY.md, IDENTITY.md, SOUL.md) lives at
        // `Config::agent_workspace_dir(alias)` instead.
        let data_dir = rei_dir.join("data");
        fs::create_dir_all(&data_dir).await.with_context(|| {
            format!(
                "Failed to create data directory: {}",
                data_dir.display().to_string()
            )
        })?;
        // Legacy alias retained for clarity in the struct initializer
        // and existing field assignments below.
        let workspace_dir = data_dir;

        // `<install>/shared/` — root workspace shared across every agent
        // on the host. Holds skills, skill bundles, and other content
        // not scoped to a single agent. Per-agent state still lives at
        // `<install>/agents/<alias>/workspace/`.
        let shared_dir = rei_dir.join("shared");
        fs::create_dir_all(&shared_dir).await.with_context(|| {
            format!(
                "Failed to create shared workspace directory: {}",
                shared_dir.display()
            )
        })?;

        fs::create_dir_all(&rei_dir)
            .await
            .with_context(|| config_dir_creation_error(&rei_dir))?;

        if let Some(contents) = persisted_contents {
            // Warn if config file is world-readable (may contain API keys)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&config_path).await
                    && meta.permissions().mode() & 0o004 != 0
                {
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(
                            crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                            ::reiclaw_log::Action::Note
                        )
                        .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                        &format!(
                            "Config file {:?} is world-readable (mode {:o}). \
                             Consider restricting with: chmod 600 {:?}",
                            config_path,
                            meta.permissions().mode() & 0o777,
                            config_path
                        )
                    );
                }
            }

            // Parse with TOML directly: serde_ignored can erase nested values
            // carrying #[serde(default)]. The raw root also feeds validation.
            let raw_root = toml::from_str::<toml::Value>(&contents).ok();

            // Retired-section tombstone: serde silently drops the unknown
            // nested section (no `deny_unknown_fields`), so deployments
            // carrying it keep parsing; the shared helper records one
            // structured warning per retired section so the retirement is
            // visible instead of a silent no-op. Runs BEFORE the
            // composition hard-error gate so a config with both problems
            // still surfaces the tombstone WARN even though the load then
            // fails. Sunset: this tombstone is a compatibility shim and
            // will be removed in a later announced window.
            let retired_section_warnings =
                crate::validation_warnings::retired_section_tombstones(&contents)
                    .into_iter()
                    .chain(crate::validation_warnings::retired_field_tombstones(
                        &contents,
                    ))
                    .collect::<Vec<_>>();

            // `composition` hard-error gate. The key is brand-new (no
            // release predating it can legitimately carry a value this
            // binary didn't ship), so an invalid value is an operator
            // typo, never a legacy artifact. Without this gate the
            // resilient loader below would silently drop the key and
            // resolve absent → `full`, widening the assembled tool
            // surface past what the operator asked for. Validity is
            // decided by the enum's own deserializer (single source of
            // truth for accepted values); the message names the
            // documented set including the `legacy` alias, which the raw
            // serde error omits.
            if let Some(raw_composition) = raw_root.as_ref().and_then(|v| v.get("composition"))
                && <crate::composition::Composition as serde::Deserialize>::deserialize(
                    raw_composition.clone(),
                )
                .is_err()
            {
                ::reiclaw_log::record!(
                    ERROR,
                    ::reiclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::reiclaw_log::Action::Reject
                    )
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "value": raw_composition.to_string(),
                    })),
                    "config.toml `composition` value is invalid; refusing to load"
                );
                anyhow::bail!(
                    "config.toml `composition` value {} is invalid; expected one of {}",
                    raw_composition,
                    crate::composition::DOCUMENTED_VALUES
                );
            }

            // Malformed current-schema sections retain a repair path. The resilient path
            // degrades (dropping invalid blocks to defaults); security-critical
            // drops are recorded on `degraded_security` for exposure gating.
            // Unsupported versions always fail; strict validation lives in
            // `rei config validate`.
            let salvage = crate::current::parse_current_salvaged(&contents)?;
            let mut config: Config = salvage.config;
            config.degraded_security = salvage.dropped_security;
            config.degraded_sections = salvage.dropped;
            // Ensure the built-in default auto_approve entries are always
            // present on a `risk_profiles.default` entry that already
            // exists. When a user
            // specifies `auto_approve` in their TOML (e.g. to add a
            // custom tool), serde replaces the default list instead of
            // merging — this re-adds the framework defaults so safe
            // tools like `weather` and `calculator` keep their
            // auto-approve status.
            //
            // Users who want to require approval for a default tool can
            // add it to `always_ask`, which takes precedence over
            // `auto_approve` in the approval decision (see approval/mod.rs).
            //
            // Skipped when the loaded config has no `risk_profiles.default`
            // entry: we will not synthesize a `default` alias here.
            // A config without that alias is a legitimate current shape.
            if let Some(default_profile) = config.risk_profiles.get_mut("default") {
                default_profile.ensure_default_auto_approve();
            }

            // Detect unknown top-level config keys by comparing the raw
            // TOML table keys against what Config actually deserializes.
            // This replaces the previous serde_ignored-based approach which
            // had false-positive issues with #[serde(default)] nested structs.
            for key in Self::unknown_keys(&contents) {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::reiclaw_log::Action::Note
                    )
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"key": key})),
                    "Unknown config key ignored: \"\". Check config.toml for typos or deprecated options."
                );
            }
            // Unknown provider families are dropped by serde, so an alias
            // created under a typo'd or unsupported family silently vanishes
            // on reload while agents.*.model_provider still references it.
            for entry in Self::unknown_provider_families(&contents) {
                let (kind, family) = entry.split_once('.').unwrap_or(("models", entry.as_str()));
                let reference = if kind == "models" {
                    "any agents.*.model_provider referencing them will fail to resolve; \
                     run `rei providers` for valid family names"
                } else {
                    "references to its aliases will fail to resolve"
                };
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::reiclaw_log::Action::Note
                    )
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"kind": kind, "family": family})),
                    &format!(
                        "[providers.{kind}.{family}] section dropped: not a known {kind} \
                        provider family. Its aliases will not load and {reference}."
                    )
                );
            }
            // Extra child tables under a known model-provider alias are also
            // erased by serde. If the child carries provider-shaped fields,
            // call out the likely mistaken nesting instead of letting the
            // parent alias look like an ordinary unfinished provider.
            for entry in Self::extra_nested_model_provider_tables(&contents) {
                let family = entry.family.as_str();
                let alias = entry.alias.as_str();
                let nested = entry.nested.as_str();
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::reiclaw_log::Action::Note
                    )
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "family": family,
                        "alias": alias,
                        "nested": nested,
                    })),
                    &format!(
                        "[providers.models.{}.{}.{}] section ignored: \
                         model-provider fields must live directly under \
                         [providers.models.{}.{}]. Move the nested keys up one level.",
                        family, alias, nested, family, alias
                    )
                );
            }
            // Set computed paths that are skipped during serialization
            config.config_path = config_path.clone();
            config.data_dir = workspace_dir;
            config.loaded_from = Some(config_path.clone());

            // Ensure each configured skill-bundle's resolved directory
            // exists on disk so the bundle has somewhere for skills to
            // land immediately. Idempotent.
            let install_root = config.install_root_dir();
            for alias in config.skill_bundles.keys().cloned().collect::<Vec<_>>() {
                if let Ok(dir) =
                    crate::skill_bundles::resolve_directory(&config, &install_root, &alias)
                    && let Err(e) = std::fs::create_dir_all(&dir)
                {
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(
                            crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                            ::reiclaw_log::Action::Note
                        )
                        .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                        &format!(
                            "skill-bundle '{alias}' directory creation failed at {}: {e}",
                            dir.display().to_string()
                        )
                    );
                }
            }

            let store = crate::secrets::SecretStore::new(&rei_dir, config.secrets.encrypt);
            config.onepassword_reference_snapshots =
                ExternalRefSnapshots::from_map(collect_onepassword_reference_snapshots(&config)?);
            // Decrypt all #[secret]-annotated fields via Configurable derive
            config = tokio::task::spawn_blocking(move || {
                config.decrypt_secrets(&store)?;
                Ok::<_, anyhow::Error>(config)
            })
            .await
            .context("Config secret decryption task failed")??;

            // Apply REI_<lowercase_path> env-var overrides. Hard-errors
            // on any unresolvable path — no silent ignores. Tracks overridden
            // paths and per-path pre-override snapshots so save() can mask
            // env-injected values back to the original on-disk state.
            let applied = crate::env_overrides::apply_env_overrides(&mut config)?;
            config.clear_dirty();
            config.env_overridden_paths = EnvOverriddenPaths::from_set(applied.paths);
            config.pre_override_snapshots = PreOverrideSnapshots::from_map(applied.snapshots);
            config.bind_env_override_identities();
            config.retired_surface_warnings = retired_section_warnings
                .into_iter()
                .chain(applied.tombstone_warnings)
                .collect();

            // Validation must NOT prevent the daemon from booting. If
            // it did, a single broken agent reference would lock the
            // operator out of `/config` — the only place they can fix
            // it. Demote to a startup warning; the gateway and dashboard
            // still come up so the user can navigate to the bad section
            // and repair it.
            if let Err(e) = config.validate() {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::reiclaw_log::Action::Note
                    )
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                    "[system] config has validation errors — booting anyway so you \
                     can fix them via /config or `rei config set`"
                );
            }
            ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(crate::schema::CONFIG_SCHEMA_LOG_TARGET, ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"path": config.config_path.display().to_string(), "workspace": config.data_dir.display().to_string(), "source": resolution_source.as_str(), "initialized": true})), "Config loaded");
            Ok(config)
        } else {
            let mut config = Config {
                config_path: config_path.clone(),
                data_dir: workspace_dir,
                ..Config::default()
            };
            // A brand-new install starts on the minimal companion
            // composition. This bootstrap and the shipped config
            // templates (dev container, production container images,
            // Kubernetes sample) are the places a fresh config file is
            // produced, and all of them pin the key explicitly.
            // Existing installs are never migrated: an absent
            // `composition` field keeps resolving as `full`, so upgrades
            // do not silently change the tool surface.
            config.composition = Some(crate::composition::Composition::Minimal);
            // Save defaults FIRST so env-injected values never reach the
            // freshly-created config file. Env overrides apply post-save to
            // populate the in-memory Config for the running process.
            config.save().await?;
            // This value just wrote the file; mark provenance so later full
            // saves of the same value stay legitimate.
            config.loaded_from = Some(config_path.clone());

            // Restrict permissions on newly created config file (may contain API keys)
            #[cfg(unix)]
            {
                use std::{fs::Permissions, os::unix::fs::PermissionsExt};
                let _ = fs::set_permissions(&config_path, Permissions::from_mode(0o600)).await;
            }

            let applied = crate::env_overrides::apply_env_overrides(&mut config)?;
            config.clear_dirty();
            config.env_overridden_paths = EnvOverriddenPaths::from_set(applied.paths);
            config.pre_override_snapshots = PreOverrideSnapshots::from_map(applied.snapshots);
            config.bind_env_override_identities();
            config.retired_surface_warnings = applied.tombstone_warnings;

            // Same boot-resilience as the load-existing branch above:
            // a fresh-init config can't realistically fail validation,
            // but if it does we still want the daemon up.
            if let Err(e) = config.validate() {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::reiclaw_log::Action::Note
                    )
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                    "[system] freshly-initialized config has validation errors — \
                     booting anyway so you can fix them via /config"
                );
            }
            ::reiclaw_log::record!(INFO, ::reiclaw_log::Event::new(crate::schema::CONFIG_SCHEMA_LOG_TARGET, ::reiclaw_log::Action::Note).with_attrs(::serde_json::json!({"path": config.config_path.display().to_string(), "workspace": config.data_dir.display().to_string(), "source": resolution_source.as_str(), "initialized": true})), "Config loaded");
            Ok(config)
        }
    }
}
