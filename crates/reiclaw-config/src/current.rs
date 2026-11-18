//! Current-schema parsing and fail-closed configuration boundaries.
//!
//! For syntactically valid TOML, this fork accepts only explicitly versioned V3
//! configuration; older, newer, missing or invalid versions never become defaults.
//! Malformed TOML retains the degraded diagnostic path, while malformed current
//! sections can be recovered in memory with their existing security exposure gates.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::schema::Config;

/// The only schema version this binary reads and writes.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

pub(crate) const RETIRED_SOP_CONFIG_MESSAGE: &str = "[sop] is retired: the SOP engine, definition loader, authoring surface and \
     procedure_v1 path were removed. Review and remove [sop] and ZEROCLAW_sop__* \
     overrides explicitly; legacy approval and guard settings are not migrated \
     to current policy or Tachi permissions.";

/// A pre-write refusal to discard retired SOP approval or guard settings.
#[derive(Debug)]
pub struct RetiredSopConfigError;

impl std::fmt::Display for RetiredSopConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(RETIRED_SOP_CONFIG_MESSAGE)
    }
}

impl std::error::Error for RetiredSopConfigError {}

/// Preserve a rejecting parse boundary without retaining editable SOP settings.
pub(crate) fn deserialize_retired_sop_config<'de, D>(
    deserializer: D,
) -> Result<std::collections::HashMap<String, ()>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _ = serde::de::IgnoredAny::deserialize(deserializer)?;
    Err(serde::de::Error::custom(RetiredSopConfigError))
}

/// Reject the old root before parsing or salvage can discard its authority.
/// Inspect only key presence, never the submitted values or member identities.
pub(crate) fn reject_retired_sop_config(table: &toml::Table) -> Result<()> {
    if table.contains_key("sop") {
        return Err(RetiredSopConfigError.into());
    }
    Ok(())
}

/// Channel kinds the owner retired on 2026-11-18 (ZeroClaw no longer reads
/// email or Discord archives). Unlike the older tombstoned transports, a
/// config that still declares or references one is refused outright: the
/// sections carry credentials and archive/ingest authority that must not be
/// discarded silently.
pub const FAIL_CLOSED_RETIRED_CHANNELS: &[&str] = &["email", "discord"];

/// A pre-write refusal of a retired channel declaration or reference. Like
/// [`RetiredSopConfigError`] it names only the channel kind, never values.
#[derive(Debug)]
pub struct RetiredChannelConfigError(&'static str);

impl std::fmt::Display for RetiredChannelConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Channel `{kind}` is retired: ZeroClaw no longer reads {kind} sources. \
             Remove every [channels.{kind}.*] section and every `{kind}.<alias>` \
             channel reference (agents.*.channels, peer_groups.*.channel); \
             credentials and settings are not migrated.",
            kind = self.0
        )
    }
}

impl std::error::Error for RetiredChannelConfigError {}

fn retired_channel_kind(name: &str) -> Option<&'static str> {
    let kind = name.split('.').next().unwrap_or(name).trim();
    FAIL_CLOSED_RETIRED_CHANNELS
        .iter()
        .copied()
        .find(|retired| retired.eq_ignore_ascii_case(kind))
}

/// Refuse a retired channel kind named by `name` (a bare kind or a
/// `<kind>.<alias>` reference). Never echoes the alias.
pub(crate) fn reject_retired_channel(name: &str) -> Result<()> {
    match retired_channel_kind(name) {
        Some(kind) => Err(RetiredChannelConfigError(kind).into()),
        None => Ok(()),
    }
}

/// Reject retired channel sections and references before parsing or salvage
/// can discard their credentials. Inspects only keys and channel references.
pub(crate) fn reject_retired_channel_config(value: &toml::Value) -> Result<()> {
    if let Some(channels) = value.get("channels").and_then(toml::Value::as_table) {
        for key in channels.keys() {
            reject_retired_channel(key)?;
        }
    }
    if let Some(agents) = value.get("agents").and_then(toml::Value::as_table) {
        for agent in agents.values() {
            if let Some(refs) = agent.get("channels").and_then(toml::Value::as_array) {
                for reference in refs.iter().filter_map(toml::Value::as_str) {
                    reject_retired_channel(reference)?;
                }
            }
        }
    }
    if let Some(groups) = value.get("peer_groups").and_then(toml::Value::as_table) {
        for group in groups.values() {
            if let Some(reference) = group.get("channel").and_then(toml::Value::as_str) {
                reject_retired_channel(reference)?;
            }
        }
    }
    Ok(())
}

pub(crate) struct ConfigLoadAttribution;

impl reiclaw_api::attribution::Attributable for ConfigLoadAttribution {
    fn role(&self) -> reiclaw_api::attribution::Role {
        reiclaw_api::attribution::Role::System
    }
    fn alias(&self) -> &str {
        "config"
    }
}

/// Read an explicit schema version without truncating oversized integers.
pub fn detect_version(value: &toml::Value) -> Result<u32> {
    let table = value
        .as_table()
        .context("config root must be a TOML table")?;
    let version = table.get("schema_version")
        .context("config must declare schema_version = 3; unversioned configuration is no longer supported")?
        .as_integer()
        .context("schema_version must be a positive integer")?;
    let version = u32::try_from(version)
        .context("schema_version must be a positive integer representable as u32")?;
    anyhow::ensure!(version > 0, "schema_version must be a positive integer");
    Ok(version)
}

/// A pre-write refusal of an unsupported or unspecified schema generation.
#[derive(Debug)]
pub struct UnsupportedSchemaError(String);

impl std::fmt::Display for UnsupportedSchemaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UnsupportedSchemaError {}

/// Reject unsupported generations before parsing, recovery or persistence.
pub fn require_current_schema(value: &toml::Value) -> Result<()> {
    if let Some(table) = value.as_table() {
        reject_retired_sop_config(table)?;
    }
    crate::providers::reject_retired_model_provider_config(value)?;
    reject_retired_channel_config(value)?;
    let version = detect_version(value).map_err(|err| UnsupportedSchemaError(err.to_string()))?;
    if version != CURRENT_SCHEMA_VERSION {
        return Err(UnsupportedSchemaError(format!(
            "schema_version {version} is unsupported; this binary accepts only schema_version {CURRENT_SCHEMA_VERSION}. The configuration was not converted or rewritten"
        )).into());
    }
    Ok(())
}

/// Strict current-schema parse for repair tooling and explicit config input.
pub fn parse_current(input: &str) -> Result<Config> {
    let _attribution = ::reiclaw_log::attribution_span!(&ConfigLoadAttribution).entered();
    let value: toml::Value = toml::from_str(input).context("failed to parse config TOML")?;
    require_current_schema(&value)?;
    value
        .try_into()
        .context("config failed to deserialize as current schema")
}

/// Current-schema recovery with its degradation report retained on Config.
pub fn parse_current_resilient(input: &str) -> Result<Config> {
    let ResilientLoad {
        mut config,
        dropped,
        dropped_security,
    } = parse_current_salvaged(input)?;
    config.degraded_security = dropped_security;
    config.degraded_sections = dropped;
    Ok(config)
}

/// Top-level keys whose silent loss could *weaken* security posture: dropping
/// a malformed one to its `Default` may grant a broader posture than intended.
/// Salvage still drops them (so the daemon boots) but logs ERROR and reports
/// them in [`ResilientLoad::dropped_security`] for exposure gating.
pub const SECURITY_CRITICAL_KEYS: &[&str] = &["security", "risk_profiles", "peer_groups"];

pub const WHOLE_CONFIG_SENTINEL: &str = "<entire-config>";

/// Result of recovering malformed sections in current configuration.
#[derive(Debug, Clone, Default)]
pub struct ResilientLoad {
    /// Loaded config: every section that parsed, `Default` for any dropped.
    pub config: Config,
    /// Non-security paths dropped during salvage (logged WARN).
    pub dropped: Vec<String>,
    /// [`SECURITY_CRITICAL_KEYS`] sections dropped to `Default` (logged ERROR).
    /// Non-empty means the running posture may be weaker than intended.
    pub dropped_security: Vec<String>,
}

/// Recover malformed current sections without accepting another schema generation.
pub fn parse_current_salvaged(input: &str) -> Result<ResilientLoad> {
    let _attribution = ::reiclaw_log::attribution_span!(&ConfigLoadAttribution).entered();
    let value: toml::Value = match toml::from_str(input) {
        Ok(value) => value,
        Err(err) => {
            ::reiclaw_log::record!(
                ERROR,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({ "error": format!("{err:#}") })),
                "config TOML could not be parsed; starting on explicitly degraded defaults for repair"
            );
            return Ok(ResilientLoad {
                config: Config::default(),
                dropped: Vec::new(),
                dropped_security: vec![WHOLE_CONFIG_SENTINEL.to_string()],
            });
        }
    };
    require_current_schema(&value)?;
    Ok(deserialize_resilient(value))
}

/// Deserialize a current-schema `toml::Value` into `Config`, never failing.
/// Strict first; on failure prune broken channel aliases, channel types, then
/// top-level sections (each → `Default`), so only the broken blocks are lost.
fn deserialize_resilient(value: toml::Value) -> ResilientLoad {
    if let Ok(config) = value.clone().try_into::<Config>() {
        return ResilientLoad {
            config,
            dropped: Vec::new(),
            dropped_security: Vec::new(),
        };
    }

    let mut salvaged = value;
    let mut dropped: Vec<String> = Vec::new();
    prune_bad_channel_aliases(&mut salvaged, &mut dropped);
    prune_bad_channel_types(&mut salvaged, &mut dropped);
    prune_bad_provider_aliases(&mut salvaged, &mut dropped);
    prune_bad_top_level_sections(&mut salvaged, &mut dropped);

    let mut whole_config_lost = false;
    let config = salvaged.try_into::<Config>().unwrap_or_else(|err| {
        // Nothing in the root table is individually salvageable (e.g. a
        // non-table root). Boot on defaults so repair surfaces are reachable.
        whole_config_lost = true;
        ::reiclaw_log::record!(
            ERROR,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({ "error": format!("{err:#}") })),
            "config could not be salvaged section-by-section; starting on defaults \
             so it can be repaired"
        );
        Config::default()
    });

    let mut dropped_security: Vec<String> = Vec::new();
    let mut dropped_plain: Vec<String> = Vec::new();
    // A whole-config default loses every security-critical section at once, so
    // mark it degraded even though no individual section was named in `dropped`.
    if whole_config_lost {
        dropped_security.push(WHOLE_CONFIG_SENTINEL.to_string());
    }
    for path in dropped {
        if SECURITY_CRITICAL_KEYS.contains(&path.as_str()) {
            dropped_security.push(path);
        } else {
            dropped_plain.push(path);
        }
    }

    for path in &dropped_plain {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({ "dropped_config": path })),
            &format!(
                "config section `{path}` is invalid and was skipped so the daemon can \
                 start; fix the block and reload to re-enable it"
            )
        );
    }
    for path in &dropped_security {
        ::reiclaw_log::record!(
            ERROR,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({ "dropped_security_config": path })),
            &format!(
                "SECURITY-CRITICAL config section `{path}` is invalid and was reset to \
                 its default so the daemon can boot; the running posture may be WEAKER \
                 than intended — repair `{path}` and reload before trusting this instance. \
                 Run `zeroclaw config validate` to see the precise parse error, or fix it \
                 via the gateway config editor at `/api/config`"
            )
        );
    }

    ResilientLoad {
        config,
        dropped: dropped_plain,
        dropped_security,
    }
}

/// Drop top-level `[section]`s that block deserialization (each → `Default`).
/// Two probes: drop a single key if its removal validates the whole config;
/// else drop every key that fails to deserialize in isolation (catches
/// multiple independent offenders the joint probe can't). Appends to `dropped`.
fn prune_bad_top_level_sections(value: &mut toml::Value, dropped: &mut Vec<String>) {
    if value.as_table().is_none() {
        return;
    }
    if value.clone().try_into::<Config>().is_ok() {
        return;
    }

    let keys: Vec<String> = value
        .as_table()
        .expect("root is a table")
        // toml::Value tables preserve insertion order, so drops are reported
        // in TOML declaration order — predictable for operators reading logs.
        .keys()
        .cloned()
        .collect();
    for key in &keys {
        let root = value.as_table_mut().expect("root is a table");
        let Some(removed) = root.remove(key) else {
            continue;
        };
        if value.clone().try_into::<Config>().is_ok() {
            dropped.push(key.clone());
            return;
        }
        value
            .as_table_mut()
            .expect("root is a table")
            .insert(key.clone(), removed);
    }

    for key in keys {
        let still_present = value.as_table().and_then(|root| root.get(&key)).cloned();
        let Some(section) = still_present else {
            continue;
        };
        if top_level_section_is_invalid(&key, &section) {
            value.as_table_mut().expect("root is a table").remove(&key);
            dropped.push(key);
        }
    }
}

/// True when top-level `[<key>]`, wrapped alone, fails to deserialize.
fn top_level_section_is_invalid(key: &str, section: &toml::Value) -> bool {
    let mut root = toml::value::Table::new();
    root.insert(key.to_string(), section.clone());
    toml::Value::Table(root).try_into::<Config>().is_err()
}

fn prune_bad_channel_aliases(value: &mut toml::Value, dropped: &mut Vec<String>) {
    let Some(channels) = value
        .as_table_mut()
        .and_then(|root| root.get_mut("channels"))
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };

    for (chan_type, aliases) in channels.iter_mut() {
        let Some(alias_table) = aliases.as_table_mut() else {
            continue;
        };
        let invalid: Vec<String> = alias_table
            .iter()
            .filter(|(_, v)| channel_alias_is_invalid(chan_type, v))
            .map(|(k, _)| k.clone())
            .collect();
        for alias in invalid {
            alias_table.remove(&alias);
            dropped.push(format!("channels.{chan_type}.{alias}"));
        }
    }
}

fn prune_bad_provider_aliases(value: &mut toml::Value, dropped: &mut Vec<String>) {
    let Some(provider_kinds) = value
        .as_table_mut()
        .and_then(|root| root.get_mut("providers"))
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };

    // Non-table nodes where a kind/family map is required (e.g.
    // `[providers.models] ollama = "oops"`) would otherwise still sink the
    // whole section in prune_bad_top_level_sections. Drop just the node.
    let scalar_kinds: Vec<String> = provider_kinds
        .iter()
        .filter(|(_, v)| !v.is_table())
        .map(|(k, _)| k.clone())
        .collect();
    for kind in scalar_kinds {
        provider_kinds.remove(&kind);
        dropped.push(format!("providers.{kind}"));
    }

    for (kind, families) in provider_kinds.iter_mut() {
        let family_table = families.as_table_mut().expect("scalar kinds pruned above");
        let scalar_families: Vec<String> = family_table
            .iter()
            .filter(|(_, v)| !v.is_table())
            .map(|(k, _)| k.clone())
            .collect();
        for family in scalar_families {
            family_table.remove(&family);
            dropped.push(format!("providers.{kind}.{family}"));
        }
        for (family, aliases) in family_table.iter_mut() {
            let alias_table = aliases
                .as_table_mut()
                .expect("scalar families pruned above");
            let invalid: Vec<String> = alias_table
                .iter()
                .filter(|(_, v)| provider_alias_is_invalid(kind, family, v))
                .map(|(k, _)| k.clone())
                .collect();
            for alias in invalid {
                alias_table.remove(&alias);
                dropped.push(format!("providers.{kind}.{family}.{alias}"));
            }
        }
    }
}

/// True when `[providers.<kind>.<family>.<alias>]`, wrapped alone, fails to
/// deserialize. Unknown families pass (serde ignores them); only a
/// known-family alias with bad field data is invalid.
fn provider_alias_is_invalid(kind: &str, family: &str, alias_value: &toml::Value) -> bool {
    let mut inner = toml::value::Table::new();
    inner.insert("probe".to_string(), alias_value.clone());
    let mut family_table = toml::value::Table::new();
    family_table.insert(family.to_string(), toml::Value::Table(inner));
    let mut kind_table = toml::value::Table::new();
    kind_table.insert(kind.to_string(), toml::Value::Table(family_table));
    let mut root = toml::value::Table::new();
    root.insert("providers".to_string(), toml::Value::Table(kind_table));
    toml::Value::Table(root).try_into::<Config>().is_err()
}

/// Drop each `[channels.<type>]` block still blocking the load after alias
/// pruning (e.g. a scalar where a table is required). Drops only the offending
/// type, never the whole `[channels]` section. Appends `channels.<type>`.
fn prune_bad_channel_types(value: &mut toml::Value, dropped: &mut Vec<String>) {
    let Some(channel_types) = value
        .as_table()
        .and_then(|root| root.get("channels"))
        .and_then(toml::Value::as_table)
        .map(|chans| chans.keys().cloned().collect::<Vec<_>>())
    else {
        return;
    };

    for chan_type in channel_types {
        if channels_section_is_valid(value) {
            return;
        }
        let Some(removed) = value
            .as_table_mut()
            .and_then(|root| root.get_mut("channels"))
            .and_then(toml::Value::as_table_mut)
            .and_then(|chans| chans.remove(&chan_type))
        else {
            continue;
        };
        if channels_section_is_valid(value) {
            dropped.push(format!("channels.{chan_type}"));
        } else {
            value
                .as_table_mut()
                .and_then(|root| root.get_mut("channels"))
                .and_then(toml::Value::as_table_mut)
                .expect("channels is a table")
                .insert(chan_type, removed);
        }
    }
}

/// True when `value`'s `[channels]` section deserializes cleanly in isolation.
fn channels_section_is_valid(value: &toml::Value) -> bool {
    let Some(channels) = value
        .as_table()
        .and_then(|root| root.get("channels"))
        .cloned()
    else {
        return true;
    };
    let mut root = toml::value::Table::new();
    root.insert("channels".to_string(), channels);
    toml::Value::Table(root).try_into::<Config>().is_ok()
}

/// True when `[channels.<type>.<alias>]`, wrapped alone, fails to deserialize.
fn channel_alias_is_invalid(chan_type: &str, alias_value: &toml::Value) -> bool {
    let mut inner = toml::value::Table::new();
    inner.insert("probe".to_string(), alias_value.clone());
    let mut type_table = toml::value::Table::new();
    type_table.insert(chan_type.to_string(), toml::Value::Table(inner));
    let mut channels = toml::value::Table::new();
    channels.insert("channels".to_string(), toml::Value::Table(type_table));
    toml::Value::Table(channels).try_into::<Config>().is_err()
}

/// Gate writes to existing files on the supported version; never rewrite here.
pub fn ensure_disk_at_current_version(path: &Path) -> Result<()> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read config at {}", path.display()));
        }
    };
    let value = toml::from_str(&raw).context("failed to parse config TOML for version check")?;
    require_current_schema(&value)
        .with_context(|| format!("refusing to modify config at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_channel_alias_is_dropped_not_fatal() {
        // A webhook alias with a mistyped port must not abort the load.
        let raw = r#"
schema_version = 3

[channels.webhook.fakehook]
enabled = true
port = "not-a-port"
"#;
        let cfg = parse_current_resilient(raw).unwrap();
        assert!(
            !cfg.channels.webhook.contains_key("fakehook"),
            "invalid alias must be pruned"
        );
    }

    #[test]
    fn partial_telegram_alias_survives_salvage() {
        // A Telegram alias with no `bot_token` (e.g. just created via
        // create_map_key, then round-tripped through save_dirty's
        // prune_empty_leaves, which strips the empty string) must survive
        // salvage instead of being dropped: `bot_token` now has
        // `#[serde(default)]`, so a missing token deserializes the same as
        // an explicit `bot_token = ""`. Runtime safety is enforced
        // separately by `validate_bot_token` when `enabled = true`.
        let raw = r#"
schema_version = 3

[channels.telegram.default]
enabled = true
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            load.config.channels.telegram.contains_key("default"),
            "a partial (tokenless) alias must survive salvage, got {:?}",
            load.config.channels.telegram.keys().collect::<Vec<_>>()
        );
        assert!(
            load.dropped.is_empty(),
            "a partial (tokenless) alias must not be reported as dropped, got {:?}",
            load.dropped
        );
    }

    #[test]
    fn corrupt_telegram_alias_is_still_dropped_and_recorded() {
        // Guard that salvage still prunes genuine garbage: a `bot_token`
        // with the wrong type (int instead of string) is a real type error,
        // not merely a missing field, and must still be dropped with the
        // exact path recorded so `doctor` can name it (see zeroclaw-runtime's
        // check_degraded_sections).
        let raw = r#"
schema_version = 3

[channels.telegram.bad]
enabled = true
bot_token = 42
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            !load.config.channels.telegram.contains_key("bad"),
            "type-corrupt alias must be pruned, got {:?}",
            load.config.channels.telegram.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            load.dropped,
            vec!["channels.telegram.bad"],
            "dropped list must pin the exact malformed section path, got {:?}",
            load.dropped
        );
    }

    #[test]
    fn complete_telegram_alias_survives() {
        // Companion to partial_telegram_alias_survives_salvage and
        // corrupt_telegram_alias_is_still_dropped_and_recorded: a complete
        // [channels.telegram.default] (bot_token present) must survive
        // intact and must not appear in `dropped`.
        let raw = r#"
schema_version = 3

[channels.telegram.default]
enabled = true
bot_token = "t"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            load.config.channels.telegram.contains_key("default"),
            "a complete alias must survive salvage"
        );
        assert!(
            load.dropped.is_empty(),
            "a complete alias must not be reported as dropped, got {:?}",
            load.dropped
        );
    }

    #[test]
    fn valid_provider_aliases_survive_broken_sibling() {
        // Repro for the zerocode "all providers vanish after restart" report:
        // one malformed provider alias must not take the whole [providers]
        // section (and every other provider) down with it.
        let raw = r#"
schema_version = 3

[providers.models.ollama.ai]
model = "qwen3:30b"

[providers.models.custom.rag_bot]
uri = "http://localhost:8000/v1"
model = "m"

[providers.models.custom.broken]
uri = "http://localhost:9000/v1"
model = "m"
temperature = "hot"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert_eq!(load.dropped, vec!["providers.models.custom.broken"]);
        assert!(
            load.config.providers.models.find("ollama", "ai").is_some(),
            "valid alias in another family must survive"
        );
        assert!(
            load.config
                .providers
                .models
                .find("custom", "rag_bot")
                .is_some(),
            "valid sibling alias must survive"
        );
        assert!(
            load.config
                .providers
                .models
                .find("custom", "broken")
                .is_none(),
            "only the malformed alias is pruned"
        );
    }

    #[test]
    fn provider_pruner_never_panics_on_non_table_shapes() {
        // Array-of-tables where a family map is expected, scalar [providers],
        // array alias value. The salvage path is the daemon's never-fail
        // loader, and prune_bad_provider_aliases carries expect() calls that
        // rely on the scalar pre-passes; pin that invariant here.
        for raw in [
            "schema_version = 3\nproviders = 3\n",
            "schema_version = 3\n[[providers.models.ollama]]\nmodel = \"x\"\n",
            "schema_version = 3\n[providers.models.ollama]\nai = [1, 2]\n",
            "schema_version = 3\n[providers.models]\nollama = [1]\n",
        ] {
            let _ = parse_current_salvaged(raw).unwrap();
        }
    }

    #[test]
    fn scalar_provider_nodes_pruned_without_sinking_section() {
        // A scalar where a family/kind table is required must drop only
        // that node, not the whole [providers] section.
        let raw = r#"
schema_version = 3

[providers.models]
ollama = "oops"

[providers.models.custom.rag_bot]
uri = "http://localhost:8000/v1"
model = "m"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert_eq!(load.dropped, vec!["providers.models.ollama"]);
        assert!(
            load.config
                .providers
                .models
                .find("custom", "rag_bot")
                .is_some(),
            "valid alias must survive a scalar sibling family"
        );
    }

    #[test]
    fn valid_alias_survives_broken_sibling() {
        let raw = r#"
schema_version = 3

[channels.webhook.broken]
enabled = true
port = "not-a-port"

[channels.webhook.good]
enabled = true
port = 8091
"#;
        let cfg = parse_current_resilient(raw).unwrap();
        assert!(
            cfg.channels.webhook.contains_key("good"),
            "valid sibling must be kept"
        );
        assert!(
            !cfg.channels.webhook.contains_key("broken"),
            "invalid sibling must be pruned"
        );
    }

    #[test]
    fn broken_non_channel_section_falls_back_to_default() {
        // A type mismatch outside the channel maps must NOT abort the daemon:
        // the section is dropped to its default so the operator can repair it.
        let raw = r#"
schema_version = 3

[heartbeat]
enabled = "not-a-bool"
"#;
        let cfg = parse_current_resilient(raw).unwrap();
        // `[heartbeat]` reverted to its serde default; load did not panic.
        assert!(!cfg.heartbeat.enabled);
        assert_eq!(cfg.heartbeat.interval_minutes, 30);
        assert_eq!(cfg.degraded_sections, vec!["heartbeat"]);
        assert!(cfg.degraded_security.is_empty());
    }

    #[test]
    fn unparseable_config_falls_back_to_defaults() {
        // Not even valid TOML — the daemon still boots on defaults so the
        // operator can reach a repair surface and overwrite the file.
        let cfg = parse_current_resilient("this is not valid TOML {{{").unwrap();
        assert_eq!(cfg.schema_version, Config::default().schema_version);
        assert_eq!(cfg.degraded_security, vec![WHOLE_CONFIG_SENTINEL]);
    }

    #[test]
    fn unparseable_config_marks_whole_config_degraded() {
        // Whole-config loss loses every security-critical section at once, so it
        // must mark the posture degraded — otherwise the serving gate has no
        // signal and boots a defaulted security posture silently.
        let load = parse_current_salvaged("this is not valid TOML {{{").unwrap();
        assert!(
            load.dropped_security
                .iter()
                .any(|p| p == WHOLE_CONFIG_SENTINEL),
            "unparseable config must degrade security posture, got {:?}",
            load.dropped_security
        );
    }

    #[test]
    fn unsalvageable_root_marks_whole_config_degraded() {
        // A root that is not a table cannot be salvaged section-by-section; the
        // final deserialize fallback defaults the whole config and must mark it.
        let raw = "schema_version = 3\nthis_is_a_bare_top_level = \"value\"\n[\n";
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            !load.dropped_security.is_empty(),
            "an unsalvageable root must degrade security posture, got {:?}",
            load.dropped_security
        );
    }

    #[test]
    fn strict_path_still_errors_for_tooling() {
        // `parse_current` stays strict — repair tooling needs the error.
        let raw = r#"
schema_version = 3

[channels.webhook.fakehook]
enabled = true
port = "not-a-port"
"#;
        assert!(
            parse_current(raw).is_err(),
            "strict path must surface the defect for repair tooling"
        );
    }

    #[test]
    fn broken_security_section_is_reported_as_degraded() {
        let raw = r#"
schema_version = 3

[security]
audit = "should-be-a-table-not-a-string"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            load.dropped_security.iter().any(|p| p == "security"),
            "malformed [security] must be reported as a security-critical drop"
        );
        assert!(
            load.dropped.is_empty(),
            "security drop must not also appear in the plain dropped list"
        );
    }

    #[test]
    fn broken_non_security_section_is_plain_drop_not_security() {
        let raw = r#"
schema_version = 3

[heartbeat]
enabled = "not-a-bool"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            load.dropped.iter().any(|p| p == "heartbeat"),
            "malformed [heartbeat] must be a plain drop"
        );
        assert!(
            load.dropped_security.is_empty(),
            "a non-security section must never be flagged security-critical"
        );
    }

    #[test]
    fn broken_channel_type_block_is_dropped_not_fatal() {
        let raw = r#"
schema_version = 3

[channels]
webhook = "oops-this-should-be-a-table"

[channels.telegram.main]
enabled = true
bot_token = "t"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            load.dropped.iter().any(|p| p == "channels.webhook"),
            "the broken whole-type block must be dropped, got {:?}",
            load.dropped
        );
        assert!(
            load.config.channels.telegram.contains_key("main"),
            "valid sibling channel type must survive a broken-type drop"
        );
    }

    #[test]
    fn multiple_independent_bad_sections_all_dropped() {
        let raw = r#"
schema_version = 3

[heartbeat]
enabled = "not-a-bool"

[backup]
enabled = "also-not-a-bool"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            load.dropped.iter().any(|p| p == "heartbeat"),
            "first offender must be dropped, got {:?}",
            load.dropped
        );
        assert!(
            load.dropped.iter().any(|p| p == "backup"),
            "second offender must be dropped, got {:?}",
            load.dropped
        );
    }

    #[test]
    fn multiple_bad_sections_one_security_critical() {
        let raw = r#"
schema_version = 3

[security]
audit = "should-be-a-table-not-a-string"

[heartbeat]
enabled = "not-a-bool"
"#;
        let load = parse_current_salvaged(raw).unwrap();
        assert!(
            load.dropped_security.iter().any(|p| p == "security"),
            "malformed [security] must be classified security-critical, got {:?}",
            load.dropped_security
        );
        assert!(
            load.dropped.iter().any(|p| p == "heartbeat"),
            "malformed [heartbeat] must be a plain drop, got {:?}",
            load.dropped
        );
        assert!(
            !load.dropped.iter().any(|p| p == "security"),
            "security drop must not also appear in the plain dropped list"
        );
    }

    #[test]
    fn retained_provider_configuration_keeps_auth_and_shared_alias_fields() {
        let raw = r#"
schema_version = 3
[providers.models.anthropic.bedrock]
api_key = "private-anthropic-key"
[providers.models.openai.azure]
model = "gpt-4o"
[providers.models.groq.telnyx]
api_key = "private-openrouter-key"
[providers.models.gemini.gemini_cli]
auth_mode = "o_auth"
oauth_project = "retained-project"
oauth_client_id = "private-client-id"
oauth_client_secret = "private-client-secret"
[providers.models.llamacpp.kilocli]
model = "retained-model"
[providers.models.lmstudio.kilo_cli]
model = "anthropic/claude-sonnet-4-6"
[providers.models.custom.compatible]
uri = "https://api.example.invalid/v1"
"#;
        let config = parse_current(raw).unwrap();
        assert_eq!(config.providers.models.iter_entries().count(), 7);
        assert_eq!(
            config.providers.models.anthropic["bedrock"]
                .base
                .api_key
                .as_deref(),
            Some("private-anthropic-key")
        );
        let gemini = &config.providers.models.gemini["gemini_cli"];
        assert_eq!(gemini.auth_mode, Some(crate::schema::AuthMode::OAuth));
        assert_eq!(gemini.oauth_project.as_deref(), Some("retained-project"));
        assert_eq!(
            gemini.oauth_client_secret.as_deref(),
            Some("private-client-secret")
        );
        let serialized = toml::to_string(&config).unwrap();
        let reloaded = parse_current(&serialized).unwrap();
        assert_eq!(reloaded.providers.models.iter_entries().count(), 7);
        assert_eq!(
            reloaded.providers.models.gemini["gemini_cli"].oauth_client_secret,
            gemini.oauth_client_secret
        );
    }

    #[test]
    fn retired_email_and_discord_fail_closed_without_exposing_values() {
        for kind in FAIL_CLOSED_RETIRED_CHANNELS {
            for body in [
                format!("[channels.{kind}.private_alias]\nenabled = false\n"),
                format!("[channels.{kind}.private_alias]\nbot_token = 'private-test-key'\n"),
                format!("[channels]\n{kind} = {{}}\n"),
                format!("[agents.owner]\nchannels = ['{kind}.private_alias']\n"),
                format!("[agents.owner]\nchannels = ['telegram.main', '{kind}']\n"),
                format!("[peer_groups.crew]\nchannel = '{kind}.private_alias'\n"),
            ] {
                let raw = format!("schema_version = 3\n{body}");
                for error in [
                    parse_current(&raw).unwrap_err(),
                    parse_current_salvaged(&raw).unwrap_err(),
                ] {
                    assert!(error.is::<RetiredChannelConfigError>(), "{kind}: {error:#}");
                    let message = format!("{error:#}");
                    assert!(message.contains(&format!("Channel `{kind}` is retired")));
                    assert!(!message.contains("private-test-key"));
                    assert!(!message.contains("private_alias"));
                }
            }
        }
        // A retained channel that merely mentions the word is not refused.
        let ok = "schema_version = 3\n[channels.webhook.email]\nenabled = false\n\
                  [agents.owner]\nchannels = ['webhook.email']\n";
        assert!(parse_current(ok).is_ok());
    }

    #[test]
    fn retired_sop_shapes_hard_fail_without_exposing_policy() {
        for body in [
            "[sop]\n",
            "sop = false\n",
            "sop = []\n",
            "[sop.approval.groups.private_group]\nmembers = ['private-member']\n",
        ] {
            let raw = format!("schema_version = 3\n{body}");
            for error in [
                parse_current(&raw).unwrap_err(),
                parse_current_salvaged(&raw).unwrap_err(),
            ] {
                assert!(error.is::<RetiredSopConfigError>());
                assert_eq!(error.to_string(), RETIRED_SOP_CONFIG_MESSAGE);
            }
        }
    }

    #[test]
    fn retired_provider_families_block_all_current_selectors_without_credential_leaks() {
        use crate::providers::{ModelProviders, retired_model_provider_names};
        for family in retired_model_provider_names() {
            for body in [
                format!(
                    "[providers.models.\"{family}\".private_alias]\napi_key = 'private-test-key'\n"
                ),
                format!("[agents.owner]\nmodel_provider = '{family}.private_alias'\n"),
                format!("[agents.owner]\nclassifier_provider = '{family}.private_alias'\n"),
                format!("[providers.models.openai.home]\nfallback = ['{family}.private_alias']\n"),
            ] {
                let raw = format!("schema_version = 3\n{body}");
                for error in [
                    parse_current(&raw).unwrap_err(),
                    parse_current_salvaged(&raw).unwrap_err(),
                ] {
                    let message = format!("{error:#}");
                    assert!(message.contains("is retired"));
                    assert!(!message.contains("private-test-key"));
                    assert!(!message.contains("private_alias"));
                }
            }
            let raw = format!("[\"{family}\".private_alias]\napi_key = 'private-test-key'\n");
            let error = toml::from_str::<ModelProviders>(&raw)
                .unwrap_err()
                .to_string();
            assert!(error.contains("is retired"));
            assert!(!error.contains("private-test-key"));
            let json =
                serde_json::json!({family: {"private_alias": {"api_key": "private-test-key"}}});
            assert!(
                serde_json::from_value::<ModelProviders>(json)
                    .unwrap_err()
                    .to_string()
                    .contains("is retired")
            );
            let reference = serde_json::json!(format!("{family}.private_alias"));
            assert!(
                serde_json::from_value::<crate::providers::ModelProviderRef>(reference)
                    .unwrap_err()
                    .to_string()
                    .contains("is retired")
            );
        }
    }
}
