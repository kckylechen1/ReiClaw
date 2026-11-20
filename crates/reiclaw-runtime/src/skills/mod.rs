pub mod skill_http;
pub mod skill_tool;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod audit;
pub mod bundle;
pub mod constants;
pub mod document;
pub mod frontmatter;
pub mod reference;
pub mod scaffold;
pub mod service;

pub use bundle::{BundleError, BundleSummary};
pub use document::{DocumentParseError, SkillDocument};
pub use frontmatter::SkillFrontmatter;
pub use reference::{SkillRef, SkillRefError};
pub use scaffold::{ScaffoldError, ScaffoldOptions};
pub use service::{
    EffectiveSkill, EffectiveSkillSet, RemoveMode, ServiceError, SkillOrigin, SkillSummary,
    SkillsService,
};

/// A skill is a user-defined or community-built capability.
/// Skills live in `~/.rei/workspace/skills/<name>/SKILL.md`
/// and can include tool definitions, prompts, and automation scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Per-locale translations of `description`, keyed by Discord locale code
    /// (e.g. `fr`, `es-ES`, `ja`). Consumed by slash-capable channels to
    /// localize the command description; empty for unlocalized skills. Declared
    /// in SKILL.toml under `[skill]` as `description_localizations`.
    #[serde(default)]
    pub description_localizations: BTreeMap<String, String>,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub tools: Vec<SkillTool>,
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Typed slash-command options a `slash`-tagged skill exposes (e.g. on
    /// Discord). Empty for skills that take no structured input — slash channels
    /// then fall back to a single free-text option. See [`SkillSlashOption`].
    #[serde(default)]
    pub slash_options: Vec<SkillSlashOption>,
    #[serde(skip)]
    pub location: Option<PathBuf>,
}

/// Why the audited resolver dropped a candidate skill directory/file.
/// Carries the human-readable detail the loader already logs, so the
/// dashboard can show the same reason without re-running the audit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillDropReason {
    /// `audit_*` returned Ok(report) with findings. `summary` = report.summary();
    /// `scripts_blocked` is true when the secure-default script policy is the
    /// blocker, so consumers can offer the `skills.allow_scripts = true` hint
    /// without re-parsing the human-readable summary.
    AuditFindings {
        summary: String,
        scripts_blocked: bool,
    },
    /// `audit_*` returned Err (unauditable); String = error message.
    AuditError(String),
    ManifestParseError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DroppedSkill {
    pub name: String,
    /// `"workspace"` | `"bundle"`.
    pub origin_hint: String,
    pub reason: SkillDropReason,
    pub location: Option<PathBuf>,
}

/// One lower-precedence skill that lost its name to an earlier (higher-priority)
/// source during the agent's effective-skill dedup. Recorded for the dashboard
/// so operators can see why an assigned bundle skill is being overridden.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShadowedSkill {
    /// The name shared with (and won by) the higher-precedence skill.
    pub name: String,
    /// Origin of the lower-precedence skill: `"bundle"`.
    pub origin_hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlashOptionKind {
    String,
    Integer,
    Number,
    Boolean,
    User,
    Channel,
    Role,
    Mentionable,
}

impl SlashOptionKind {
    /// Every kind, in the order surfaces should offer them. Walked (not
    /// restated) by every registry consumer.
    pub const ALL: [Self; 8] = [
        Self::String,
        Self::Integer,
        Self::Number,
        Self::Boolean,
        Self::User,
        Self::Channel,
        Self::Role,
        Self::Mentionable,
    ];

    /// The canonical `type` token written in frontmatter.
    pub fn manifest_name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::User => "user",
            Self::Channel => "channel",
            Self::Role => "role",
            Self::Mentionable => "mentionable",
        }
    }

    /// Predefined `choices` apply only to string/integer/number options.
    pub fn supports_choices(self) -> bool {
        match self {
            Self::String | Self::Integer | Self::Number => true,
            Self::Boolean | Self::User | Self::Channel | Self::Role | Self::Mentionable => false,
        }
    }

    /// `min`/`max` numeric bounds apply only to integer/number options.
    pub fn supports_numeric_bounds(self) -> bool {
        match self {
            Self::Integer | Self::Number => true,
            Self::String
            | Self::Boolean
            | Self::User
            | Self::Channel
            | Self::Role
            | Self::Mentionable => false,
        }
    }

    /// `min_length`/`max_length` bounds apply only to string options.
    pub fn supports_length_bounds(self) -> bool {
        match self {
            Self::String => true,
            Self::Integer
            | Self::Number
            | Self::Boolean
            | Self::User
            | Self::Channel
            | Self::Role
            | Self::Mentionable => false,
        }
    }

    /// The wire-facing capability row for this kind, consumed by API surfaces.
    pub fn descriptor(self) -> SlashOptionKindDescriptor {
        SlashOptionKindDescriptor {
            manifest_name: self.manifest_name().to_string(),
            supports_choices: self.supports_choices(),
            supports_numeric_bounds: self.supports_numeric_bounds(),
            supports_length_bounds: self.supports_length_bounds(),
        }
    }
}

/// Serialized capability row for one [`SlashOptionKind`], as published to
/// surfaces (the web dashboard mirrors this shape). Built by walking
/// [`SlashOptionKind::ALL`]; never hand-authored.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct SlashOptionKindDescriptor {
    pub manifest_name: String,
    pub supports_choices: bool,
    pub supports_numeric_bounds: bool,
    pub supports_length_bounds: bool,
}

/// The full registry, produced by exhaustively walking [`SlashOptionKind::ALL`].
pub fn slash_option_kinds() -> Vec<SlashOptionKindDescriptor> {
    SlashOptionKind::ALL
        .into_iter()
        .map(SlashOptionKind::descriptor)
        .collect()
}

/// A typed option a `slash`-tagged skill exposes on its slash command. Shaped
/// after the Discord Application Command Option model but channel-agnostic; a
/// slash-capable channel maps `kind` to its wire option type. Declared in
/// SKILL.toml under `[[skill.slash_options]]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillSlashOption {
    pub name: String,
    pub description: String,
    /// Per-locale translations of `description`, keyed by Discord locale code.
    /// Empty for unlocalized options. Declared under
    /// `[[skill.slash_options]]` as `description_localizations`.
    #[serde(default)]
    pub description_localizations: BTreeMap<String, String>,
    /// `string` | `integer` | `number` | `boolean` | `user` | `channel` |
    /// `role` | `mentionable`. Unknown values are dropped by the channel.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    /// Predefined choices (string/integer/number options only). The `value` is
    /// kept as text and coerced to the option's type by the channel.
    #[serde(default)]
    pub choices: Vec<SkillSlashChoice>,
    /// Inclusive bounds for integer/number options.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Length bounds for string options.
    #[serde(default)]
    pub min_length: Option<u32>,
    #[serde(default)]
    pub max_length: Option<u32>,
}

/// A predefined choice for a typed slash option.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillSlashChoice {
    pub name: String,
    pub value: String,
}

impl ::reiclaw_api::attribution::Attributable for Skill {
    fn role(&self) -> ::reiclaw_api::attribution::Role {
        ::reiclaw_api::attribution::Role::Skill
    }
    fn alias(&self) -> &str {
        &self.name
    }
}

/// A tool defined by a skill (shell command, HTTP call, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    pub name: String,
    pub description: String,
    /// "shell", "http", "script", "builtin", "mcp"
    pub kind: String,
    /// The command/URL/script to execute (unused for builtin/mcp kinds)
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: HashMap<String, String>,
    /// For `kind = "builtin"`: the name of the built-in tool to delegate to.
    /// For `kind = "mcp"`: the prefixed MCP tool name `{server}__{tool}`
    /// (e.g. `images__generate`).
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default, alias = "default_args")]
    pub locked_args: HashMap<String, String>,
    /// For `kind = "shell"` / `kind = "script"`: maximum execution time in
    /// seconds before the command is killed. Unset falls back to the built-in
    /// `SKILL_SHELL_TIMEOUT_SECS` (60s) default; long-running skills (e.g. a
    /// build pipeline) raise it via `timeout_secs` in SKILL.toml.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Skill manifest parsed from SKILL.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillManifest {
    skill: SkillMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forge: Option<ForgeMetadata>,
    #[serde(default)]
    tools: Vec<SkillTool>,
    #[serde(default)]
    prompts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillMeta {
    name: String,
    description: String,
    #[serde(default)]
    description_localizations: BTreeMap<String, String>,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    prompts: Vec<String>,
    #[serde(default)]
    slash_options: Vec<SkillSlashOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgeMetadata {
    /// Upstream URL the skill was integrated from.
    #[serde(default)]
    source: Option<String>,
    /// Upstream owner (GitHub user / org).
    #[serde(default)]
    owner: Option<String>,
    /// Primary language reported by the source (or `"unknown"`).
    #[serde(default)]
    language: Option<String>,
    /// `true` if the upstream repo carries a license file.
    #[serde(default)]
    license: Option<bool>,
    /// Upstream star count at integration time.
    #[serde(default)]
    stars: Option<u64>,
    /// Upstream `updated_at` timestamp formatted `YYYY-MM-DD`, or
    /// `"unknown"` if the integrator could not resolve one.
    #[serde(default)]
    updated_at: Option<String>,
    /// Runtime/version requirements declared by the integrator.
    #[serde(default)]
    requirements: BTreeMap<String, toml::Value>,
    #[serde(default)]
    metadata: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Default)]
struct SkillMarkdownMeta {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    author: Option<String>,
    tags: Vec<String>,
    /// Typed slash-command options from the nested `slash_options:` frontmatter
    /// block. Parsed by the shared helper in `document` (not the flat scanner)
    /// so a SKILL.md skill can drive native Discord slash commands — parity with
    /// SKILL.toml's `[[skill.slash_options]]`.
    slash_options: Vec<SkillSlashOption>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Emit a user-visible warning when a skill directory is skipped due to audit
/// findings. When `scripts_blocked` is set and `allow_scripts` is `false`, the
/// message includes actionable remediation guidance so users know how to enable
/// their skill.
fn warn_skipped_skill(path: &Path, summary: &str, scripts_blocked: bool, allow_scripts: bool) {
    if scripts_blocked && !allow_scripts {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
            &format!(
                "skipping skill directory {}: {summary}. \
             To allow script files in skills, set `skills.allow_scripts = true` in your config.",
                path.display().to_string()
            )
        );
        eprintln!(
            "warning: skill '{}' was skipped because it contains script files. \
             Set `skills.allow_scripts = true` in your reiclaw config to enable it.",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        );
    } else {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
            &format!(
                "skipping insecure skill directory {}: {summary}",
                path.display().to_string()
            )
        );
    }
}

fn warn_metadata_drift(skill_dir: &Path, toml_skill: &Skill, md_path: &Path) {
    if !md_path.exists() {
        return;
    }
    let Ok(md_content) = std::fs::read_to_string(md_path) else {
        return;
    };
    let parsed = parse_skill_markdown(&md_content);
    let dir_name = skill_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");

    if let Some(ref md_name) = parsed.meta.name
        && md_name != &toml_skill.name
    {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
            &format!(
                "skill '{}': name mismatch between TOML ('{}') and SKILL.md ('{}')",
                dir_name, toml_skill.name, md_name
            )
        );
    }
    if let Some(ref md_desc) = parsed.meta.description {
        let md_desc = md_desc.trim();
        if !md_desc.is_empty() && md_desc != ">-" && md_desc != toml_skill.description.trim() {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                &format!(
                    "skill '{}': description mismatch between TOML and SKILL.md — TOML takes precedence",
                    dir_name
                )
            );
        }
    }
}

/// Infer the directory/file stem a dropped/loaded skill is named after when its
/// manifest can't be (or wasn't) read.
fn dir_stem(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Load all skills from the workspace skills directory
pub fn load_skills(workspace_dir: &Path) -> Vec<Skill> {
    load_workspace_skills(workspace_dir, false).0
}

/// Load skills using runtime config values (preferred at runtime).
pub fn load_skills_with_config(
    workspace_dir: &Path,
    config: &reiclaw_config::schema::Config,
) -> Vec<Skill> {
    load_skills_with_config_audited(workspace_dir, config).0
}

/// Like [`load_skills_with_config`] but also returns the audit-dropped
/// candidates the resolver skipped, so the dashboard can surface them
pub fn load_skills_with_config_audited(
    workspace_dir: &Path,
    config: &reiclaw_config::schema::Config,
) -> (Vec<Skill>, Vec<DroppedSkill>) {
    load_workspace_skills(workspace_dir, config.skills.allow_scripts)
}

pub fn load_skills_for_agent(
    workspace_dir: &Path,
    config: &reiclaw_config::schema::Config,
    agent_alias: &str,
) -> Vec<Skill> {
    load_skills_for_agent_audited(workspace_dir, config, agent_alias).0
}

/// [`load_skills_for_agent`] plus the audit-dropped and shadowed candidates the
/// resolver skipped, so the dashboard can surface them without re-auditing or
/// re-walking
pub fn load_skills_for_agent_audited(
    workspace_dir: &Path,
    config: &reiclaw_config::schema::Config,
    agent_alias: &str,
) -> (Vec<Skill>, Vec<DroppedSkill>, Vec<ShadowedSkill>) {
    let (mut skills, mut dropped) = load_skills_with_config_audited(workspace_dir, config);
    let mut shadows: Vec<ShadowedSkill> = Vec::new();
    let Some(agent) = config.agent(agent_alias) else {
        return (skills, dropped, shadows);
    };
    if agent.skill_bundles.is_empty() {
        return (skills, dropped, shadows);
    }
    let install_root = config.install_root_dir();
    let allow_scripts = config.skills.allow_scripts;
    // Workspace skills and earlier bundles keep precedence by name.
    let mut seen: HashSet<String> = skills.iter().map(|s| s.name.clone()).collect();
    for bundle_alias in &agent.skill_bundles {
        let bundle = match config.skill_bundles.get(bundle_alias) {
            Some(b) => b,
            None => {
                ::reiclaw_log::record!(WARN, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_outcome(::reiclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"agent": agent_alias, "bundle": bundle_alias, "bundle_alias": bundle_alias})), "skipping skill bundle: [skill_bundles.] is not configured");
                continue;
            }
        };
        let dir = match reiclaw_config::skill_bundles::resolve_directory(
            config,
            &install_root,
            bundle_alias,
        ) {
            Ok(d) => d,
            Err(e) => {
                ::reiclaw_log::record!(WARN, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_outcome(::reiclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"agent": agent_alias, "bundle": bundle_alias, "e": e.to_string()})), "skipping skill bundle: ");
                continue;
            }
        };
        let (bundle_skills, bundle_dropped) = load_skills_from_directory(&dir, allow_scripts);
        dropped.extend(bundle_dropped.into_iter().map(|mut d| {
            d.origin_hint = "bundle".into();
            d
        }));
        for skill in bundle_skills {
            if !bundle.admits_skill(&skill.name) {
                continue;
            }
            // First-write wins so workspace skills override bundle skills
            // with the same name (legacy agents who edited a workspace
            // copy keep their override after a bundle is assigned).
            if seen.contains(&skill.name) {
                // This bundle skill lost the name to an earlier source.
                // Record the loser keyed to the winner's name so the
                // dashboard can badge the winning skill.
                shadows.push(ShadowedSkill {
                    name: skill.name.clone(),
                    origin_hint: "bundle".into(),
                });
            } else {
                seen.insert(skill.name.clone());
                skills.push(skill);
            }
        }
    }
    (skills, dropped, shadows)
}

pub fn load_skills_for_agent_from_config(
    config: &reiclaw_config::schema::Config,
    agent_alias: &str,
) -> Vec<Skill> {
    load_skills_for_agent_from_config_audited(config, agent_alias).0
}

/// [`load_skills_for_agent_from_config`] plus the audit-dropped and shadowed
/// candidates the resolver skipped — the dashboard's source for the
/// skipped-audit banner and shadow badges
pub fn load_skills_for_agent_from_config_audited(
    config: &reiclaw_config::schema::Config,
    agent_alias: &str,
) -> (Vec<Skill>, Vec<DroppedSkill>, Vec<ShadowedSkill>) {
    load_skills_for_agent_audited(
        &config.agent_workspace_dir(agent_alias),
        config,
        agent_alias,
    )
}

fn load_workspace_skills(
    workspace_dir: &Path,
    allow_scripts: bool,
) -> (Vec<Skill>, Vec<DroppedSkill>) {
    let skills_dir = workspace_dir.join("skills");
    load_skills_from_directory(&skills_dir, allow_scripts)
}

/// Read and audit every candidate on each load. Operator edits and script
/// policy changes are visible immediately to runtime and service consumers.
pub fn load_skills_from_directory(
    skills_dir: &Path,
    allow_scripts: bool,
) -> (Vec<Skill>, Vec<DroppedSkill>) {
    let mut skills = Vec::new();
    let mut dropped = Vec::new();
    if !skills_dir.exists() {
        return (skills, dropped);
    }

    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return (skills, dropped);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        match audit::audit_skill_directory_with_options(
            &path,
            audit::SkillAuditOptions { allow_scripts },
        ) {
            Ok(report) if report.is_clean() => {}
            Ok(report) => {
                let summary = report.summary();
                let scripts_blocked = report.scripts_blocked;
                warn_skipped_skill(&path, &summary, scripts_blocked, allow_scripts);
                dropped.push(DroppedSkill {
                    name: dir_stem(&path),
                    origin_hint: "workspace".into(),
                    reason: SkillDropReason::AuditFindings {
                        summary,
                        scripts_blocked,
                    },
                    location: Some(path.clone()),
                });
                continue;
            }
            Err(err) => {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                    &format!(
                        "skipping unauditable skill directory {}: {err}",
                        path.display().to_string()
                    )
                );
                dropped.push(DroppedSkill {
                    name: dir_stem(&path),
                    origin_hint: "workspace".into(),
                    reason: SkillDropReason::AuditError(err.to_string()),
                    location: Some(path.clone()),
                });
                continue;
            }
        }

        // Try SKILL.toml first, then manifest.toml (registry format), then SKILL.md
        let skill_toml_path = path.join("SKILL.toml");
        let manifest_toml_path = path.join("manifest.toml");
        let md_path = path.join("SKILL.md");

        let toml_path = if skill_toml_path.exists() {
            Some(skill_toml_path)
        } else if manifest_toml_path.exists() {
            Some(manifest_toml_path)
        } else {
            None
        };

        if let Some(toml_path) = toml_path {
            match load_skill_toml(&toml_path) {
                Ok(skill) => {
                    warn_metadata_drift(&path, &skill, &md_path);
                    skills.push(skill);
                }
                Err(e) => {
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                            .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "path": toml_path.display().to_string(),
                                "error": format!("{}", e),
                            })),
                        "failed to load SKILL.toml — skill directory skipped"
                    );
                    dropped.push(DroppedSkill {
                        name: dir_stem(&path),
                        origin_hint: "workspace".into(),
                        reason: SkillDropReason::ManifestParseError(format!("{e}")),
                        location: Some(path.clone()),
                    });
                }
            }
        } else if md_path.exists()
            && let Ok(skill) = load_skill_md(&md_path, &path)
        {
            skills.push(skill);
        }
    }

    (skills, dropped)
}

fn load_skill_toml(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let manifest: SkillManifest = toml::from_str(&content)?;

    // Merge prompts from both locations: inside the [skill] table (natural
    // location for per-skill prompts) and at the manifest root (historical
    // location). Previously, prompts placed inside [skill] were silently
    // dropped because SkillMeta had no `prompts` field.
    let mut prompts = manifest.skill.prompts;
    prompts.extend(manifest.prompts);

    Ok(Skill {
        name: manifest.skill.name,
        description: manifest.skill.description,
        description_localizations: manifest.skill.description_localizations,
        version: manifest.skill.version,
        author: manifest.skill.author,
        tags: manifest.skill.tags,
        tools: manifest.tools,
        prompts,
        slash_options: manifest.skill.slash_options,
        location: Some(path.to_path_buf()),
    })
}

/// Load a skill from a SKILL.md file (simpler format)
fn load_skill_md(path: &Path, dir: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let parsed = parse_skill_markdown(&content);
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(Skill {
        name: parsed.meta.name.unwrap_or(name),
        description: parsed
            .meta
            .description
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| extract_description(&parsed.body)),
        // SKILL.md frontmatter carries no localizations.
        description_localizations: Default::default(),
        version: parsed.meta.version.unwrap_or_else(default_version),
        author: parsed.meta.author,
        tags: parsed.meta.tags,
        tools: Vec::new(),
        prompts: vec![parsed.body],
        slash_options: parsed.meta.slash_options,
        location: Some(path.to_path_buf()),
    })
}

struct ParsedSkillMarkdown {
    meta: SkillMarkdownMeta,
    body: String,
}

fn parse_skill_markdown(content: &str) -> ParsedSkillMarkdown {
    if let Some((frontmatter, body)) = split_skill_frontmatter(content) {
        let meta = parse_simple_frontmatter(&frontmatter);
        return ParsedSkillMarkdown { meta, body };
    }

    ParsedSkillMarkdown {
        meta: SkillMarkdownMeta::default(),
        body: content.to_string(),
    }
}

/// Lightweight YAML-like frontmatter parser for simple `key: value` pairs.
/// Replaces `serde_yaml` to avoid pulling in the full YAML parser (~30KB)
/// for a struct with only 5 optional string fields.
fn parse_simple_frontmatter(s: &str) -> SkillMarkdownMeta {
    let mut meta = SkillMarkdownMeta::default();
    let mut collecting_tags = false;
    let mut collecting_multiline: Option<String> = None;
    let mut multiline_parts: Vec<String> = Vec::new();

    let flush_multiline = |key: &str, parts: &[String], meta: &mut SkillMarkdownMeta| {
        let joined = parts.join(" ");
        let val = joined.trim();
        if !val.is_empty() {
            match key {
                "description" => meta.description = Some(val.to_string()),
                "name" => meta.name = Some(val.to_string()),
                _ => {}
            }
        }
    };

    for line in s.lines() {
        // Collect indented continuation lines for YAML block scalars (>- or |)
        if let Some(ref key) = collecting_multiline {
            // A blank/whitespace-only line is a paragraph break *inside* the
            // block scalar, not a terminator — keep collecting. Only a
            // non-indented, non-empty line (a real next key) ends the scalar.
            if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
                multiline_parts.push(line.trim().to_string());
                continue;
            }
            flush_multiline(key, &multiline_parts, &mut meta);
            collecting_multiline = None;
            multiline_parts.clear();
        }

        // Handle YAML list items under `tags:` (e.g. "  - parser")
        if collecting_tags {
            let trimmed = line.trim();
            if let Some(item) = trimmed.strip_prefix("- ") {
                let tag = item.trim().trim_matches('"').trim_matches('\'');
                if !tag.is_empty() {
                    meta.tags.push(tag.to_string());
                }
                continue;
            }
            // Non-list-item line → stop collecting tags
            collecting_tags = false;
        }
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim().trim_matches('"').trim_matches('\'');
        // YAML block scalar indicators — collect continuation lines
        if val == ">-" || val == ">" || val == "|" || val == "|-" {
            collecting_multiline = Some(key.to_string());
            multiline_parts.clear();
            continue;
        }
        match key {
            "name" => meta.name = Some(val.to_string()),
            "description" => meta.description = Some(val.to_string()),
            "version" => meta.version = Some(val.to_string()),
            "author" => meta.author = Some(val.to_string()),
            "tags" => {
                if val.is_empty() {
                    // YAML block list follows on subsequent lines
                    collecting_tags = true;
                } else {
                    // Inline: [a, b, c] or comma-separated
                    let val = val.trim_start_matches('[').trim_end_matches(']');
                    meta.tags = val
                        .split(',')
                        .map(|t| t.trim().trim_matches('"').trim_matches('\'').to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                }
            }
            _ => {}
        }
    }
    if let Some(ref key) = collecting_multiline {
        flush_multiline(key, &multiline_parts, &mut meta);
    }
    // The one nested field. Parsed by the shared helper so the loader and the
    // service (`SkillDocument`) read `slash_options` identically — no second
    // nested parser to drift.
    meta.slash_options = document::parse_slash_options(s);
    meta
}

fn split_skill_frontmatter(content: &str) -> Option<(String, String)> {
    let normalized = content.replace("\r\n", "\n");
    let rest = normalized.strip_prefix("---\n")?;
    if let Some(idx) = rest.find("\n---\n") {
        let frontmatter = rest[..idx].to_string();
        let body = rest[idx + 5..].to_string();
        return Some((frontmatter, body));
    }
    if let Some(frontmatter) = rest.strip_suffix("\n---") {
        return Some((frontmatter.to_string(), String::new()));
    }
    None
}

fn extract_description(content: &str) -> String {
    content
        .lines()
        .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        .unwrap_or("No description")
        .trim()
        .to_string()
}

fn append_xml_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
}

fn write_xml_text_element(out: &mut String, indent: usize, tag: &str, value: &str) {
    for _ in 0..indent {
        out.push(' ');
    }
    out.push('<');
    out.push_str(tag);
    out.push('>');
    append_xml_escaped(out, value);
    out.push_str("</");
    out.push_str(tag);
    out.push_str(">\n");
}

fn resolve_skill_location(skill: &Skill, workspace_dir: &Path) -> PathBuf {
    skill.location.clone().unwrap_or_else(|| {
        workspace_dir
            .join("skills")
            .join(&skill.name)
            .join("SKILL.md")
    })
}

fn render_skill_location(skill: &Skill, workspace_dir: &Path, prefer_relative: bool) -> String {
    let location = resolve_skill_location(skill, workspace_dir);
    if prefer_relative && let Ok(relative) = location.strip_prefix(workspace_dir) {
        return display_skill_location(relative);
    }
    display_skill_location(&location)
}

fn display_skill_location(path: &Path) -> String {
    path.display().to_string()
}

/// Build the "Available Skills" system prompt section with full skill instructions.
pub fn skills_to_prompt(skills: &[Skill], workspace_dir: &Path) -> String {
    skills_to_prompt_with_mode(
        skills,
        workspace_dir,
        reiclaw_config::schema::SkillsPromptInjectionMode::Full,
    )
}

fn is_registered_skill_tool_kind(kind: &str) -> bool {
    matches!(kind, "shell" | "script" | "http" | "builtin" | "mcp")
}

fn skill_tool_is_prompt_callable(tool: &SkillTool) -> bool {
    if !is_registered_skill_tool_kind(tool.kind.as_str()) {
        return false;
    }
    match tool.kind.as_str() {
        "builtin" | "mcp" => tool.target.as_deref().is_some_and(|t| !t.trim().is_empty()),
        _ => true,
    }
}

/// Build the "Available Skills" system prompt section with configurable verbosity.
pub fn skills_to_prompt_with_mode(
    skills: &[Skill],
    workspace_dir: &Path,
    mode: reiclaw_config::schema::SkillsPromptInjectionMode,
) -> String {
    use std::fmt::Write;

    if skills.is_empty() {
        return String::new();
    }

    let mut prompt = match mode {
        reiclaw_config::schema::SkillsPromptInjectionMode::Full => String::from(
            "## Available Skills\n\n\
             Skill instructions and tool metadata are preloaded below.\n\
             Follow these instructions directly; do not read skill files at runtime unless the user asks.\n\n\
             <available_skills>\n",
        ),
        reiclaw_config::schema::SkillsPromptInjectionMode::Compact => String::from(
            "## Available Skills\n\n\
             Skill summaries are preloaded below to keep context compact.\n\
             Skill instructions are loaded on demand: call `read_skill(name)` with the skill's `<name>` when you need the full skill file.\n\
             The `location` field is included for reference.\n\n\
             <available_skills>\n",
        ),
    };

    for skill in skills {
        let _ = writeln!(prompt, "  <skill>");
        write_xml_text_element(&mut prompt, 4, "name", &skill.name);
        write_xml_text_element(&mut prompt, 4, "description", &skill.description);
        let location = render_skill_location(
            skill,
            workspace_dir,
            matches!(
                mode,
                reiclaw_config::schema::SkillsPromptInjectionMode::Compact
            ),
        );
        write_xml_text_element(&mut prompt, 4, "location", &location);

        // In Full mode, inline both instructions and tools.
        // In Compact mode, skip instructions (loaded on demand) but keep tools
        // so the LLM knows which skill tools are available.
        if matches!(
            mode,
            reiclaw_config::schema::SkillsPromptInjectionMode::Full
        ) && !skill.prompts.is_empty()
        {
            let _ = writeln!(prompt, "    <instructions>");
            for instruction in &skill.prompts {
                write_xml_text_element(&mut prompt, 6, "instruction", instruction);
            }
            let _ = writeln!(prompt, "    </instructions>");
        }

        if !skill.tools.is_empty() {
            let registered: Vec<_> = skill
                .tools
                .iter()
                .filter(|t| skill_tool_is_prompt_callable(t))
                .collect();
            let unregistered: Vec<_> = skill
                .tools
                .iter()
                .filter(|t| !skill_tool_is_prompt_callable(t))
                .collect();

            if !registered.is_empty() {
                let _ = writeln!(
                    prompt,
                    "    <callable_tools hint=\"These are registered as callable tool specs. Invoke them directly by name ({{}}__{{}}) instead of using shell.\">"
                );
                for tool in &registered {
                    let _ = writeln!(prompt, "      <tool>");
                    write_xml_text_element(
                        &mut prompt,
                        8,
                        "name",
                        // Must match the registered tool spec's name exactly
                        // (same sanitizer), or the model is told to call a name
                        // that no tool exposes
                        &crate::tools::skill_tool::composed_tool_name(&skill.name, &tool.name),
                    );
                    write_xml_text_element(&mut prompt, 8, "description", &tool.description);
                    let _ = writeln!(prompt, "      </tool>");
                }
                let _ = writeln!(prompt, "    </callable_tools>");
            }

            if !unregistered.is_empty() {
                let _ = writeln!(prompt, "    <tools>");
                for tool in &unregistered {
                    let _ = writeln!(prompt, "      <tool>");
                    write_xml_text_element(&mut prompt, 8, "name", &tool.name);
                    write_xml_text_element(&mut prompt, 8, "description", &tool.description);
                    write_xml_text_element(&mut prompt, 8, "kind", &tool.kind);
                    let _ = writeln!(prompt, "      </tool>");
                }
                let _ = writeln!(prompt, "    </tools>");
            }
        }

        let _ = writeln!(prompt, "  </skill>");
    }

    prompt.push_str("</available_skills>");
    prompt
}

pub fn skills_to_tools(
    skills: &[Skill],
    security: std::sync::Arc<crate::security::SecurityPolicy>,
) -> Vec<Box<dyn reiclaw_api::tool::Tool>> {
    skills_to_tools_with_context(skills, security, &[])
}

/// Build the wrapper for a skill-declared `builtin` / `mcp` elevation, or
/// `None` when it must not exist.
///
/// The elevation registry is deliberately unfiltered — a skill has to be able
/// to *name* a tool the agent cannot call directly, or elevation would have no
/// purpose. What it must not do is *reach* one the profile never granted.
///
/// The check has to happen here rather than at registration, because a wrapper
/// is named after its skill: `is_tool_allowed("research__fetch")` asks about a
/// name no allow-list will ever contain, and answers "denied" for a legitimate
/// wrapper while saying nothing about what it delegates to. Authority belongs
/// to the target.
fn resolve_elevated_tool(
    skill_name: &str,
    tool: &SkillTool,
    kind_label: &str,
    resolution_registry: &[std::sync::Arc<dyn reiclaw_api::tool::Tool>],
    security: &crate::security::SecurityPolicy,
) -> Option<Box<dyn reiclaw_api::tool::Tool>> {
    let Some(target_name) = tool.target.as_deref() else {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                .with_outcome(::reiclaw_log::EventOutcome::Unknown),
            &format!(
                "Skill tool {}.{} has kind='{}' but no 'target' field, skipping",
                skill_name, tool.name, kind_label
            )
        );
        return None;
    };
    // Only the allow-list gates elevation, never `excluded_tools`. The two mean
    // different things and conflating them breaks the feature: `excluded_tools`
    // says "do not hand the model this tool directly", and a scoped wrapper with
    // locked arguments is precisely the sanctioned way to reach it anyway. An
    // allow-list is the agent's authority boundary, and elevation must not cross
    // it — a skill may narrow what the agent can already do, never widen it.
    let outside_allow_list = security
        .allowed_tools
        .as_ref()
        .is_some_and(|list| !list.iter().any(|t| t == target_name));
    if outside_allow_list {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "skill": skill_name,
                    "skill_tool": tool.name,
                    "delegates_to": target_name,
                    "kind": kind_label,
                })),
            "Skill elevation refused: it delegates to a tool this profile does not allow"
        );
        return None;
    }
    match resolution_registry.iter().find(|t| t.name() == target_name) {
        Some(target) => Some(Box::new(
            crate::skills::skill_tool::SkillBuiltinTool::new(
                skill_name,
                tool,
                std::sync::Arc::clone(target),
                tool.locked_args.clone(),
            )
            .with_security(std::sync::Arc::new(security.clone())),
        )),
        None => {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                &format!(
                    "Skill tool {}.{} targets {} '{}' which was not found in the \
                     resolution registry (for MCP, use the prefixed name \
                     '{{server}}__{{tool}}' and ensure the server is connected), skipping",
                    skill_name, tool.name, kind_label, target_name
                )
            );
            None
        }
    }
}

pub fn skills_to_tools_with_context(
    skills: &[Skill],
    security: std::sync::Arc<crate::security::SecurityPolicy>,
    unfiltered_registry: &[std::sync::Arc<dyn reiclaw_api::tool::Tool>],
) -> Vec<Box<dyn reiclaw_api::tool::Tool>> {
    skills_to_tools_with_context_and_runtime(
        skills,
        security,
        unfiltered_registry,
        std::sync::Arc::new(crate::platform::NativeRuntime::new()),
    )
}

pub fn skills_to_tools_with_context_and_runtime(
    skills: &[Skill],
    security: std::sync::Arc<crate::security::SecurityPolicy>,
    unfiltered_registry: &[std::sync::Arc<dyn reiclaw_api::tool::Tool>],
    runtime: std::sync::Arc<dyn crate::platform::RuntimeAdapter>,
) -> Vec<Box<dyn reiclaw_api::tool::Tool>> {
    let mut tools: Vec<Box<dyn reiclaw_api::tool::Tool>> = Vec::new();
    for skill in skills {
        for tool in &skill.tools {
            if !is_registered_skill_tool_kind(tool.kind.as_str()) {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                    &format!(
                        "Unknown skill tool kind '{}' for {}.{}, skipping",
                        tool.kind, skill.name, tool.name
                    )
                );
                continue;
            }
            match tool.kind.as_str() {
                "shell" | "script" => {
                    let inner = crate::skills::skill_tool::SkillShellTool::new_with_runtime(
                        &skill.name,
                        tool,
                        security.clone(),
                        runtime.clone(),
                    );
                    tools.push(Box::new(reiclaw_tools::wrappers::RateLimitedTool::new(
                        inner,
                        security.clone(),
                    )));
                }
                "http" => {
                    tools.push(Box::new(
                        crate::skills::skill_http::SkillHttpTool::new(&skill.name, tool)
                            .with_security(security.clone()),
                    ));
                }
                "builtin" => {
                    if let Some(t) = resolve_elevated_tool(
                        &skill.name,
                        tool,
                        "builtin",
                        unfiltered_registry,
                        &security,
                    ) {
                        tools.push(t);
                    }
                }
                "mcp" => {
                    if let Some(t) = resolve_elevated_tool(
                        &skill.name,
                        tool,
                        "MCP",
                        unfiltered_registry,
                        &security,
                    ) {
                        tools.push(t);
                    }
                }
                // `is_registered_skill_tool_kind` above admits only the kinds
                // dispatched here, so any other kind was already skipped.
                other => unreachable!("registered skill kind '{other}' not dispatched"),
            }
        }
    }
    tools
}

/// Get the skills directory path
pub fn skills_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("skills")
}

/// Initialize the skills directory with a README
pub fn init_skills_dir(workspace_dir: &Path) -> Result<()> {
    let dir = skills_dir(workspace_dir);
    std::fs::create_dir_all(&dir)?;

    let readme = dir.join("README.md");
    if !readme.exists() {
        std::fs::write(
            &readme,
            "# ReiClaw Skills\n\n\
             Each subdirectory is a skill. Create a `SKILL.toml` or `SKILL.md` file inside.\n\n\
             ## SKILL.toml format\n\n\
             ```toml\n\
             [skill]\n\
             name = \"my-skill\"\n\
             description = \"What this skill does\"\n\
             version = \"0.1.0\"\n\
             author = \"your-name\"\n\
             tags = [\"productivity\", \"automation\"]\n\n\
             [[tools]]\n\
             name = \"my_tool\"\n\
             description = \"What this tool does\"\n\
             kind = \"shell\"\n\
             command = \"echo hello\"\n\
             ```\n\n\
             ## SKILL.md format (simpler)\n\n\
             Just write a markdown file with instructions for the agent.\n\
             Optional YAML frontmatter is supported for `name`, `description`, `version`, `author`, and `tags`.\n\
             The agent will read it and follow the instructions.\n\n\
             ## Installing community skills\n\n\
             ```bash\n\
             rei skills install <source>\n\
             rei skills list\n\
             ```\n",
        )?;
    }

    Ok(())
}

pub fn is_git_source(source: &str) -> bool {
    is_git_scheme_source(source, "https://")
        || is_git_scheme_source(source, "http://")
        || is_git_scheme_source(source, "ssh://")
        || is_git_scheme_source(source, "git://")
        || is_git_scp_source(source)
}

fn is_git_scheme_source(source: &str, scheme: &str) -> bool {
    let Some(rest) = source.strip_prefix(scheme) else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return false;
    }

    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
}

fn is_git_scp_source(source: &str) -> bool {
    // SCP-like syntax accepted by git, e.g. git@host:owner/repo.git
    // Keep this strict enough to avoid treating local paths as git remotes.
    let Some((user_host, remote_path)) = source.split_once(':') else {
        return false;
    };
    if remote_path.is_empty() {
        return false;
    }
    if source.contains("://") {
        return false;
    }

    let Some((user, host)) = user_host.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !host.is_empty()
        && !user.contains('/')
        && !user.contains('\\')
        && !host.contains('/')
        && !host.contains('\\')
}

fn snapshot_skill_children(skills_path: &Path) -> Result<HashSet<PathBuf>> {
    let mut paths = HashSet::new();
    for entry in std::fs::read_dir(skills_path)? {
        let entry = entry?;
        paths.insert(entry.path());
    }
    Ok(paths)
}

fn detect_newly_installed_directory(
    skills_path: &Path,
    before: &HashSet<PathBuf>,
) -> Result<PathBuf> {
    let mut created = Vec::new();
    for entry in std::fs::read_dir(skills_path)? {
        let entry = entry?;
        let path = entry.path();
        if !before.contains(&path) && path.is_dir() {
            created.push(path);
        }
    }

    match created.len() {
        1 => Ok(created.remove(0)),
        0 => anyhow::bail!(
            "Unable to determine installed skill directory after clone (no new directory found)"
        ),
        _ => anyhow::bail!(
            "Unable to determine installed skill directory after clone (multiple new directories found)"
        ),
    }
}

fn enforce_skill_security_audit(
    skill_path: &Path,
    allow_scripts: bool,
) -> Result<audit::SkillAuditReport> {
    let report = audit::audit_skill_directory_with_options(
        skill_path,
        audit::SkillAuditOptions { allow_scripts },
    )?;
    if report.is_clean() {
        return Ok(report);
    }

    anyhow::bail!("Skill security audit failed: {}", report.summary());
}

fn remove_git_metadata(skill_path: &Path) -> Result<()> {
    let git_dir = skill_path.join(".git");
    if git_dir.exists() {
        std::fs::remove_dir_all(&git_dir)
            .with_context(|| format!("failed to remove {}", git_dir.display().to_string()))?;
    }
    Ok(())
}

fn copy_dir_recursive_secure(src: &Path, dest: &Path) -> Result<()> {
    let src_meta = std::fs::symlink_metadata(src)
        .with_context(|| format!("failed to read metadata for {}", src.display().to_string()))?;
    if src_meta.file_type().is_symlink() {
        anyhow::bail!(
            "Refusing to copy symlinked skill source path: {}",
            src.display()
        );
    }
    if !src_meta.is_dir() {
        anyhow::bail!(
            "Skill source must be a directory: {}",
            src.display().to_string()
        );
    }

    std::fs::create_dir_all(dest).with_context(|| {
        format!(
            "failed to create destination {}",
            dest.display().to_string()
        )
    })?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&src_path).with_context(|| {
            format!(
                "failed to read metadata for {}",
                src_path.display().to_string()
            )
        })?;

        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "Refusing to copy symlink within skill source: {}",
                src_path.display()
            );
        }

        if metadata.is_dir() {
            copy_dir_recursive_secure(&src_path, &dest_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&src_path, &dest_path).with_context(|| {
                format!(
                    "failed to copy skill file from {} to {}",
                    src_path.display().to_string(),
                    dest_path.display()
                )
            })?;
        }
    }

    Ok(())
}

pub fn install_local_skill_source(
    source: &str,
    skills_path: &Path,
    allow_scripts: bool,
) -> Result<(PathBuf, usize)> {
    let source_path = PathBuf::from(source);
    if !source_path.exists() {
        anyhow::bail!("Source path does not exist: {source}");
    }

    let source_path = source_path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source path {source}"))?;
    let _ = enforce_skill_security_audit(&source_path, allow_scripts)?;

    let name = source_path
        .file_name()
        .context("Source path must include a directory name")?;
    let dest = skills_path.join(name);
    if dest.exists() {
        anyhow::bail!(
            "Destination skill already exists: {}",
            dest.display().to_string()
        );
    }

    if let Err(err) = copy_dir_recursive_secure(&source_path, &dest) {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    }

    match enforce_skill_security_audit(&dest, allow_scripts) {
        Ok(report) => Ok((dest, report.files_scanned)),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(err)
        }
    }
}

pub fn install_git_skill_source(
    source: &str,
    skills_path: &Path,
    allow_scripts: bool,
) -> Result<(PathBuf, usize)> {
    let before = snapshot_skill_children(skills_path)?;
    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", source])
        .current_dir(skills_path)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git clone failed: {stderr}");
    }

    let installed_dir = detect_newly_installed_directory(skills_path, &before)?;
    remove_git_metadata(&installed_dir)?;
    match enforce_skill_security_audit(&installed_dir, allow_scripts) {
        Ok(report) => Ok((installed_dir, report.files_scanned)),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&installed_dir);
            Err(err)
        }
    }
}

// ─── Explicit git catalog installs ───────────────────────────────────────────

pub fn is_skill_name(source: &str) -> bool {
    if source.is_empty() {
        return false;
    }
    if source.contains('/') || source.contains('\\') || source.contains("..") {
        return false;
    }
    if source.contains("://") || source.contains(':') {
        return false;
    }
    if source.starts_with('.') || source.starts_with('~') {
        return false;
    }
    source
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn clone_skills_repository(target_dir: &Path, repo_url: &str) -> Result<()> {
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create catalog parent: {}",
                parent.display().to_string()
            )
        })?;
    }

    let output = Command::new("git")
        .args(["clone", "--depth", "1", repo_url])
        .arg(target_dir)
        .output()
        .context("failed to run git clone for skill catalog")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to clone skill catalog: {stderr}");
    }

    ::reiclaw_log::record!(
        INFO,
        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note),
        &format!(
            "cloned skill catalog to {}",
            target_dir.display().to_string()
        )
    );
    Ok(())
}

/// List real directory entries under an already-contained catalog `skills/`
/// root without following entry symlinks.
fn list_contained_catalog_skill_names(skills_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(skills_root) else {
        return vec![];
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Install a single skill by name from a git catalog repository.
///
/// Clones `url` into a throwaway directory, resolves `skills/<skill_name>/`
/// and installs it through the shared local-copy path (which
/// runs the security audit). No archive handling — pure `git clone`.
pub fn install_git_catalog_skill_source(
    url: &str,
    skill_name: &str,
    skills_path: &Path,
    allow_scripts: bool,
    workspace_dir: &Path,
) -> Result<(PathBuf, usize)> {
    if !is_skill_name(skill_name) {
        anyhow::bail!(format!(
            "invalid --skill name '{skill_name}': use a bare skill name (letters, digits, '-', '_')"
        ));
    }

    std::fs::create_dir_all(workspace_dir)
        .with_context(|| format!("failed to clone skill catalog {url}"))?;
    let clone_tempdir = tempfile::Builder::new()
        .prefix(".skill-catalog-")
        .tempdir_in(workspace_dir)
        .with_context(|| format!("failed to clone skill catalog {url}"))?;
    let clone_dir = clone_tempdir.path();

    // A transient catalog has no sync lifecycle, so clone it without writing
    // the persistent registry marker into the untrusted checkout. Besides
    // avoiding unnecessary state, this prevents a catalog-committed marker
    // symlink from redirecting that write to an arbitrary host file.
    clone_skills_repository(clone_dir, url)
        .with_context(|| format!("failed to clone skill catalog {url}"))?;

    (|| {
        // Establish the catalog trust boundary before looking up a selected
        // name or enumerating available names. A catalog controls `skills/`,
        // so following it before this check could inspect an arbitrary host
        // directory even when the requested skill does not exist.
        let clone_root = clone_dir.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize catalog clone {}",
                clone_dir.display()
            )
        })?;
        let skills_dir = clone_dir.join("skills");
        let skills_meta = match std::fs::symlink_metadata(&skills_dir) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(format!(
                    "skill '{skill_name}' not found in {url}: no skills/ directory, or it is empty"
                ));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read metadata for catalog skills root {}",
                        skills_dir.display()
                    )
                });
            }
        };
        if skills_meta.file_type().is_symlink() {
            anyhow::bail!(format!(
                "skill catalog {url} has a symlinked skills/ directory; refusing to inspect it"
            ));
        }
        let skills_root = skills_dir.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize catalog skills root {}",
                skills_dir.display()
            )
        })?;
        if !skills_root.starts_with(&clone_root) {
            anyhow::bail!(format!(
                "skill catalog {url} has a skills/ directory that resolves outside the cloned catalog; refusing to inspect it"
            ));
        }
        if !skills_root.is_dir() {
            anyhow::bail!(format!(
                "skill '{skill_name}' not found in {url}: no skills/ directory, or it is empty"
            ));
        }

        let skill_dir = skills_root.join(skill_name);
        let entry_meta = match std::fs::symlink_metadata(&skill_dir) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let available = list_contained_catalog_skill_names(&skills_root);
                if available.is_empty() {
                    anyhow::bail!(format!(
                        "skill '{skill_name}' not found in {url}: no skills/ directory, or it is empty"
                    ));
                }
                anyhow::bail!(format!(
                    "skill '{skill_name}' not found in {url}.\n\
                    Available skills: {available}",
                    available = available.join(", ")
                ));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read metadata for selected catalog skill {}",
                        skill_dir.display()
                    )
                });
            }
        };
        if entry_meta.file_type().is_symlink() {
            anyhow::bail!(format!(
                "skill '{skill_name}' in {url} is a symlink; catalog skills must be real directories inside the repository"
            ));
        }
        let selected = skill_dir.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize selected skill {}",
                skill_dir.display()
            )
        })?;
        if !selected.starts_with(&skills_root) {
            anyhow::bail!(format!(
                "skill '{skill_name}' in {url} resolves outside the cloned catalog; refusing to install"
            ));
        }
        if !selected.is_dir() {
            let available = list_contained_catalog_skill_names(&skills_root);
            if available.is_empty() {
                anyhow::bail!(format!(
                    "skill '{skill_name}' not found in {url}: no skills/ directory, or it is empty"
                ));
            }
            anyhow::bail!(format!(
                "skill '{skill_name}' not found in {url}.\n\
                Available skills: {available}",
                available = available.join(", ")
            ));
        }
        // `.skill-catalog-*` scratch dir, so this only fires on a broken
        // host filesystem; it is a developer diagnostic, not normal CLI output.
        let skill_dir_str = selected
            .to_str()
            .with_context(|| format!("skill path is not valid UTF-8: {}", selected.display()))?;
        install_local_skill_source(skill_dir_str, skills_path, allow_scripts)
    })()
}

#[cfg(test)]
mod tests;
