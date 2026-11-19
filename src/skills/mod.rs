#[allow(unused_imports)]
pub use reiclaw_runtime::skills::*;

use anyhow::{Context, Result};
use reiclaw_runtime::skills::{ScaffoldOptions, SkillFrontmatter, SkillsService};
use std::path::{Path, PathBuf};

pub mod creator {
    #[allow(unused_imports)]
    pub use reiclaw_runtime::skills::creator::*;
}
pub mod audit {
    #[allow(unused_imports)]
    pub use reiclaw_runtime::skills::audit::*;
}
pub mod skill_tool {
    #[allow(unused_imports)]
    pub use reiclaw_runtime::skills::skill_tool::*;
}
pub mod skill_http {
    #[allow(unused_imports)]
    pub use reiclaw_runtime::skills::skill_http::*;
}

pub async fn handle_command(
    command: crate::SkillCommands,
    config: &crate::config::Config,
) -> Result<()> {
    let workspace_dir = &config.data_dir;
    match command {
        crate::SkillCommands::List { agent, bundle } => {
            let install_root = config.install_root_dir();
            let allow_scripts = config.skills.allow_scripts;

            // Build the ordered (label, skills) groups to display.
            let mut rendered: Vec<(String, Vec<Skill>)> = Vec::new();
            let mut skipped: Vec<DroppedSkill> = Vec::new();
            if let Some(ref b) = bundle {
                // A single bundle's on-disk skills.
                let dir =
                    reiclaw_config::skill_bundles::resolve_directory(config, &install_root, b)
                        .map_err(anyhow::Error::msg)?;
                rendered.push((
                    format!("bundle: {b}"),
                    load_skills_from_directory(&dir, allow_scripts).0,
                ));
            } else if let Some(ref a) = agent {
                // Exactly what this agent loads at runtime — the same loader the
                // agent boot/loop uses (workspace + open-skills +
                // assigned bundles), so `list --agent` mirrors runtime behavior.
                if config.agent(a).is_none() {
                    anyhow::bail!("agent '{a}' is not configured");
                }
                let (skills, dropped, _shadowed) =
                    load_skills_for_agent_from_config_audited(config, a);
                skipped.extend(dropped);
                rendered.push((format!("loaded by agent '{a}'"), skills));
            } else {
                // Full inventory: every bundle, then the agent-agnostic sources
                // (global dir + open-skills). `load_skills_with_config`
                // is the same loader the old `list` used, so those rows are
                // preservedreview).
                for alias in config.skill_bundles.keys() {
                    if let Ok(dir) = reiclaw_config::skill_bundles::resolve_directory(
                        config,
                        &install_root,
                        alias,
                    ) {
                        rendered.push((
                            format!("bundle: {alias}"),
                            load_skills_from_directory(&dir, allow_scripts).0,
                        ));
                    }
                }
                let (skills, dropped) = load_skills_with_config_audited(&config.data_dir, config);
                skipped.extend(dropped);
                rendered.push((
                    "global / open-skills / plugins (not from a bundle)".to_string(),
                    skills,
                ));
            }

            let total: usize = rendered.iter().map(|(_, s)| s.len()).sum();

            if total == 0 {
                println!("No skills installed.");
                println!();
                println!("  Create one: rei skills add my-skill --bundle <alias>");
                println!("  Or install: rei skills install <source>");
            } else {
                println!("Installed skills ({count}):", count = total);
                println!();
                for (label, skills) in &rendered {
                    if skills.is_empty() {
                        continue;
                    }
                    println!("  {}", console::style(format!("[{label}]")).dim());
                    for skill in skills {
                        print_skill(skill);
                    }
                    println!();
                }
            }
            if !skipped.is_empty() {
                println!();
                println!("Skipped ({count}):", count = skipped.len());
                println!();
                for entry in &skipped {
                    let (reason, scripts_blocked) = match &entry.reason {
                        SkillDropReason::AuditFindings {
                            summary,
                            scripts_blocked,
                        } => (summary.clone(), *scripts_blocked),
                        SkillDropReason::AuditError(s) | SkillDropReason::ManifestParseError(s) => {
                            (s.clone(), false)
                        }
                    };
                    println!("  {}", console::style(&entry.name).yellow().bold());
                    println!("    Reason: {reason}");
                    if scripts_blocked && !config.skills.allow_scripts {
                        println!(
                            "    Set `skills.allow_scripts = true` in your rei config to enable it."
                        );
                    }
                }
            }
            println!();
            Ok(())
        }
        crate::SkillCommands::Audit { source } => {
            let source_path = PathBuf::from(&source);
            let target = if source_path.exists() {
                source_path
            } else {
                locate_installed_skill_dir(config, &source)?
            };

            let report = audit::audit_skill_directory_with_options(
                &target,
                audit::SkillAuditOptions {
                    allow_scripts: config.skills.allow_scripts,
                },
            )?;
            if report.is_clean() {
                println!(
                    "  {} Skill audit passed for {} ({} files scanned).",
                    console::style("✓").green().bold(),
                    target.display(),
                    report.files_scanned
                );
                return Ok(());
            }

            println!(
                "  {} Skill audit failed for {}",
                console::style("✗").red().bold(),
                target.display()
            );
            for finding in report.findings {
                println!("    - {finding}");
            }
            anyhow::bail!("Skill audit failed.".to_string());
        }
        crate::SkillCommands::Install {
            source,
            agent,
            bundle,
            no_tier_banner,
            skill,
        } => {
            println!("Installing skill from: {source}");

            let location = resolve_install_location(config, agent.as_deref(), bundle.as_deref())?;
            let skills_path = location.dir().to_path_buf();
            std::fs::create_dir_all(&skills_path)?;

            let (installed_dir, files_scanned) = if let Some(skill_name) = skill.as_deref() {
                if !is_git_source(&source) {
                    anyhow::bail!(format!(
                        "--skill <name> requires a git repository URL as the source (got '{source}')"
                    ));
                }
                install_git_catalog_skill_source(
                    &source,
                    skill_name,
                    &skills_path,
                    config.skills.allow_scripts,
                    workspace_dir,
                )
                .with_context(|| {
                    format!("failed to install skill '{skill_name}' from catalog {source}")
                })?
            } else if is_git_source(&source) {
                install_git_skill_source(&source, &skills_path, config.skills.allow_scripts)
                    .with_context(|| format!("failed to install git skill source: {source}"))?
            } else if is_registry_source(&source) {
                println!("  Resolving '{source}' from skills registry...");
                install_registry_skill_source(
                    &source,
                    &skills_path,
                    config.skills.allow_scripts,
                    workspace_dir,
                    config.skills.registry_url.as_deref(),
                    no_tier_banner,
                )
                .with_context(|| format!("failed to install skill from registry: {source}"))?
            } else if is_extra_registry_source(&source) {
                // `is_extra_registry_source` is `parse_extra_registry_source(..).is_some()`,
                // so this re-parse always succeeds. `unwrap_or_default` only guards an
                // unreachable `None` for a cosmetic label rather than panicking in the CLI.
                let registry_label = parse_extra_registry_source(&source)
                    .map(|(name, _)| name)
                    .unwrap_or_default();
                println!("  Resolving '{source}' from registry '{registry_label}'...");
                install_extra_registry_skill_source(
                    &source,
                    &skills_path,
                    config.skills.allow_scripts,
                    workspace_dir,
                    &config.skills.extra_registries,
                    no_tier_banner,
                )
                .with_context(|| format!("failed to install skill from extra registry: {source}"))?
            } else {
                install_local_skill_source(&source, &skills_path, config.skills.allow_scripts)
                    .with_context(|| format!("failed to install local skill source: {source}"))?
            };
            let status = console::style("✓").green().bold().to_string();
            let installed_path = installed_dir.display().to_string();
            let files_scanned = files_scanned.to_string();
            println!(
                "  {status} Skill installed and audited: {installed_path} ({files_scanned} files scanned)"
            );

            println!("  Security audit completed successfully.");

            // Tell the user whether the skill is in a loadable location.
            match &location {
                SkillLocation::Bundle { alias, .. } => println!(
                    "  Installed into bundle '{alias}'. Agents that list this bundle in skill_bundles will load it."
                ),
                SkillLocation::Global { .. } => println!(
                    "  Note: installed into the global skills dir, which no agent loads automatically. Re-run with --bundle <alias>, or assign a bundle to an agent, to make it loadable."
                ),
            }
            Ok(())
        }
        crate::SkillCommands::Remove {
            name,
            agent,
            bundle,
        } => {
            // Reject path traversal attempts
            if name.contains("..") || name.contains('/') || name.contains('\\') {
                anyhow::bail!("Invalid skill name: {name}");
            }
            let status = console::style("✓").green().bold().to_string();

            if let Some(ref a) = agent
                && config.agent(a).is_none()
            {
                anyhow::bail!("agent '{a}' is not configured");
            }

            // Explicit bundle: archive through the service (recoverable).
            if let Some(ref b) = bundle {
                let service = SkillsService::new(config, config.install_root_dir());
                let target = service
                    .resolve_ref(&name, Some(b))
                    .map_err(anyhow::Error::msg)?;
                service
                    .remove_skill(&target, reiclaw_runtime::skills::RemoveMode::Archive)
                    .map_err(anyhow::Error::msg)?;
                println!(
                    "  {status} Skill '{name}' removed from bundle '{b}' (archived under shared/skills/_deleted/)."
                );
                return Ok(());
            }

            // Otherwise locate the skill across bundles (+ global) and disambiguate.
            let matches = collect_skill_locations(config, &name, agent.as_deref());
            match matches.as_slice() {
                [] => anyhow::bail!("Skill not found: {name}"),
                [(label, dir)] => {
                    if let Some(alias) = label.strip_prefix("bundle:") {
                        let service = SkillsService::new(config, config.install_root_dir());
                        let target = service
                            .resolve_ref(&name, Some(alias))
                            .map_err(anyhow::Error::msg)?;
                        service
                            .remove_skill(&target, reiclaw_runtime::skills::RemoveMode::Archive)
                            .map_err(anyhow::Error::msg)?;
                        println!(
                            "  {status} Skill '{name}' removed from bundle '{alias}' (archived under shared/skills/_deleted/)."
                        );
                    } else {
                        // Global dir: plain delete with a containment guard.
                        let global_root = skills_dir(&config.data_dir);
                        let canonical_root =
                            global_root.canonicalize().unwrap_or(global_root.clone());
                        if let Ok(c) = dir.canonicalize()
                            && !c.starts_with(&canonical_root)
                        {
                            anyhow::bail!("Skill path escapes skills directory: {name}");
                        }
                        std::fs::remove_dir_all(dir)?;
                        println!("  {status} Skill '{name}' removed from the global skills dir.");
                    }
                }
                many => {
                    let locs = many
                        .iter()
                        .map(|(l, _)| l.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow::bail!(
                        "skill '{name}' exists in multiple locations ({locs}); pass --bundle to choose one"
                    );
                }
            }
            Ok(())
        }
        crate::SkillCommands::Add {
            name,
            bundle,
            description,
            license,
            author,
            version,
            category,
            no_scaffold,
            edit,
        } => handle_add(
            config,
            name,
            bundle,
            description,
            license,
            author,
            version,
            category,
            no_scaffold,
            edit,
        ),
        crate::SkillCommands::Edit { name, bundle, file } => {
            handle_edit(config, name, bundle, file)
        }
        crate::SkillCommands::Bundle { bundle_command } => match bundle_command {
            crate::SkillBundleCommands::List => handle_bundle_list(config),
            crate::SkillBundleCommands::Add { alias, directory } => {
                Box::pin(handle_bundle_add(config, alias, directory)).await
            }
            crate::SkillBundleCommands::Remove { alias, yes } => {
                Box::pin(handle_bundle_remove(config, alias, yes)).await
            }
            crate::SkillBundleCommands::Rename { from, to } => {
                Box::pin(handle_bundle_rename(config, from, to)).await
            }
            crate::SkillBundleCommands::Show { alias } => handle_bundle_show(config, alias),
        },
        crate::SkillCommands::Test { name, verbose } => {
            let results = if let Some(ref skill_name) = name {
                // Test a single skill
                let source_path = PathBuf::from(skill_name);
                let target = if source_path.exists() {
                    source_path
                } else {
                    locate_installed_skill_dir(config, skill_name)?
                };

                let r = testing::test_skill(&target, skill_name, verbose)?;
                if r.tests_run == 0 {
                    println!(
                        "  {} No TEST.sh found for skill '{}'.",
                        console::style("-").dim(),
                        skill_name,
                    );
                    return Ok(());
                }
                vec![r]
            } else {
                // Test all skills across every bundle plus the global dir.
                let install_root = config.install_root_dir();
                let mut dirs: Vec<PathBuf> = config
                    .skill_bundles
                    .keys()
                    .filter_map(|a| {
                        reiclaw_config::skill_bundles::resolve_directory(config, &install_root, a)
                            .ok()
                    })
                    .collect();
                dirs.push(skills_dir(&config.data_dir));
                testing::test_all_skills(&dirs, verbose)?
            };

            testing::print_results(&results);

            let any_failed = results.iter().any(|r| !r.failures.is_empty());
            if any_failed {
                anyhow::bail!("Some skill tests failed.");
            }
            Ok(())
        }
    }
}

enum SkillLocation {
    Bundle { alias: String, dir: PathBuf },
    Global { dir: PathBuf },
}

impl SkillLocation {
    fn dir(&self) -> &Path {
        match self {
            SkillLocation::Bundle { dir, .. } | SkillLocation::Global { dir } => dir,
        }
    }
}

/// Resolve where `skills install` should write. Precedence: an explicit
/// `--bundle`, then the target agent's single assigned bundle, then the global
/// fallback dir. `--agent` selects the target agent (default: the active agent).
fn resolve_install_location(
    config: &crate::config::Config,
    agent: Option<&str>,
    bundle: Option<&str>,
) -> Result<SkillLocation> {
    let install_root = config.install_root_dir();

    // Validate an explicit --agent up front, so a typo'd alias errors even when
    // --bundle is also given (which otherwise returns before the agent block).
    if let Some(a) = agent
        && config.agent(a).is_none()
    {
        anyhow::bail!("agent '{a}' is not configured");
    }

    // 1. An explicit bundle wins outright (mirrors `skills add`/`edit`).
    if let Some(alias) = bundle {
        if !config.skill_bundles.contains_key(alias) {
            anyhow::bail!("skill bundle '{alias}' is not configured");
        }
        let dir = reiclaw_config::skill_bundles::resolve_directory(config, &install_root, alias)
            .map_err(anyhow::Error::msg)?;
        return Ok(SkillLocation::Bundle {
            alias: alias.to_string(),
            dir,
        });
    }

    // 2. Pick the target agent: explicit `--agent`, else the active agent.
    let target_agent: Option<String> = match agent {
        Some(a) => Some(a.to_string()),
        None => config.resolved_runtime_agent_alias().map(str::to_string),
    };

    // 3. Derive the destination from that agent's assigned bundles.
    if let Some(alias) = target_agent.as_deref()
        && let Some(agent_cfg) = config.agent(alias)
    {
        match agent_cfg.skill_bundles.as_slice() {
            [one] => {
                let dir =
                    reiclaw_config::skill_bundles::resolve_directory(config, &install_root, one)
                        .map_err(anyhow::Error::msg)?;
                return Ok(SkillLocation::Bundle {
                    alias: one.clone(),
                    dir,
                });
            }
            [] => {} // no bundle assigned — fall through to the global dir
            many => {
                let bundles = many.join(", ");
                anyhow::bail!(
                    "agent '{alias}' has multiple skill bundles ({bundles}); pass --bundle to choose one",
                    bundles = bundles.as_str()
                );
            }
        }
    }

    // 4. Global fallback — installed but not auto-loaded (caller prints a note).
    Ok(SkillLocation::Global {
        dir: skills_dir(&config.data_dir),
    })
}

/// Every location (bundle dirs + the global dir) that contains a skill named
/// `name`, as `(label, skill-dir)` pairs. Bundle labels are `bundle:<alias>`;
/// the global dir is `global`. `agent_filter` restricts the bundle search to
/// the bundles assigned to that agent (and drops the global dir).
fn collect_skill_locations(
    config: &crate::config::Config,
    name: &str,
    agent_filter: Option<&str>,
) -> Vec<(String, PathBuf)> {
    let install_root = config.install_root_dir();
    let allowed: Option<Vec<String>> =
        agent_filter.and_then(|a| config.agent(a).map(|c| c.skill_bundles.clone()));

    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for alias in config.skill_bundles.keys() {
        if let Some(ref allow) = allowed
            && !allow.contains(alias)
        {
            continue;
        }
        if let Ok(dir) =
            reiclaw_config::skill_bundles::resolve_directory(config, &install_root, alias)
        {
            let candidate = dir.join(name);
            if candidate.is_dir() {
                out.push((format!("bundle:{alias}"), candidate));
            }
        }
    }
    if agent_filter.is_none() {
        let global = skills_dir(&config.data_dir).join(name);
        if global.is_dir() {
            out.push(("global".to_string(), global));
        }
    }
    out
}

/// Locate a single installed skill directory by name (across bundles + global),
/// erroring when absent or ambiguous. Used by `audit`/`test`.
fn locate_installed_skill_dir(config: &crate::config::Config, name: &str) -> Result<PathBuf> {
    let mut matches = collect_skill_locations(config, name, None);
    match matches.len() {
        0 => anyhow::bail!("Skill not found: {name}"),
        1 => Ok(matches.remove(0).1),
        _ => {
            let locs = matches
                .iter()
                .map(|(label, _)| label.clone())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "skill '{name}' exists in multiple locations ({locs}); pass an explicit path to disambiguate"
            )
        }
    }
}

/// Render one skill row for `skills list` (name + version + tools + tags).
fn print_skill(skill: &Skill) {
    println!(
        "  {} {} — {}",
        console::style(&skill.name).white().bold(),
        console::style(format!("v{}", skill.version)).dim(),
        skill.description
    );
    if !skill.tools.is_empty() {
        println!(
            "    Tools: {}",
            skill
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !skill.tags.is_empty() {
        println!("    Tags:  {tags}", tags = skill.tags.join(", "));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_add(
    config: &crate::config::Config,
    name: String,
    bundle: Option<String>,
    description: Option<String>,
    license: Option<String>,
    author: Option<String>,
    version: Option<String>,
    category: Option<String>,
    no_scaffold: bool,
    edit: bool,
) -> Result<()> {
    let install_root = config.install_root_dir();
    let service = SkillsService::new(config, install_root);
    let target = service
        .resolve_ref(&name, bundle.as_deref())
        .context("failed to resolve bundle target for skill add")?;

    let description = prompt_for_description(description)?;
    let frontmatter = SkillFrontmatter {
        name: target.name().to_string(),
        description,
        license,
        author,
        version: Some(version.unwrap_or_else(|| "0.1.0".to_string())),
        category,
        // Scaffold creates a tagless skill; tags (including the `slash` opt-in
        // slash commands) are managed in the dashboard skills editor.
        tags: Vec::new(),
        // Slash options are authored in the dashboard editor, not at scaffold time.
        slash_options: Vec::new(),
    };

    let skill_dir = service.scaffold_skill(
        &target,
        frontmatter,
        ScaffoldOptions {
            create_optional_subdirs: !no_scaffold,
            body: String::new(),
        },
    )?;

    println!(
        "Scaffolded skill {target} at {dir}",
        dir = skill_dir.display(),
        target = target
    );

    if edit {
        open_in_editor(
            &skill_dir.join(reiclaw_runtime::skills::constants::SKILL_MANIFEST_FILENAME),
        )?;
    }
    Ok(())
}

fn handle_edit(
    config: &crate::config::Config,
    name: String,
    bundle: Option<String>,
    file: Option<String>,
) -> Result<()> {
    let install_root = config.install_root_dir();
    let service = SkillsService::new(config, install_root);
    let target = service.resolve_ref(&name, bundle.as_deref())?;

    let summary = service
        .list_skills(Some(target.bundle()))?
        .into_iter()
        .find(|s| s.r#ref.name() == target.name())
        .ok_or_else(|| {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"skill_ref": target.to_string()})),
                "skill show: target ref not found"
            );
            anyhow::Error::msg(format!("skill '{target}' not found"))
        })?;

    let path = match file {
        Some(rel) => summary.directory.join(rel),
        None => summary
            .directory
            .join(reiclaw_runtime::skills::constants::SKILL_MANIFEST_FILENAME),
    };
    if !path.exists() {
        anyhow::bail!("file not found: {}", path.display());
    }
    open_in_editor(&path)
}

/// Create a skill bundle: insert the config entry, set a custom directory if
/// given, materialize the resolved directory, and persist.
async fn handle_bundle_add(
    config: &crate::config::Config,
    alias: String,
    directory: Option<String>,
) -> Result<()> {
    let mut working = config.clone();
    if !working
        .create_map_key("skill_bundles", &alias)
        .map_err(anyhow::Error::msg)?
    {
        println!(
            "skill bundle '{alias}' already exists (no change)",
            alias = alias.as_str()
        );
        return Ok(());
    }
    if let Some(dir) = directory.as_ref()
        && let Some(b) = working.skill_bundles.get_mut(&alias)
    {
        b.directory = Some(dir.clone());
    }
    working.mark_dirty(&format!("skill_bundles.{alias}"));
    let install_root = working.install_root_dir();
    match reiclaw_config::skill_bundles::resolve_directory(&working, &install_root, &alias) {
        Ok(dir) => {
            tokio::fs::create_dir_all(&dir).await.ok();
            let d = dir.display().to_string();
            println!(
                "created skill_bundles.{alias} (dir: {dir})",
                alias = alias.as_str(),
                dir = d.as_str()
            );
        }
        Err(e) => {
            let es = e.to_string();
            println!(
                "created skill_bundles.{alias} (warning: dir resolve failed: {error})",
                alias = alias.as_str(),
                error = es.as_str()
            );
        }
    }
    Box::pin(working.save_dirty())
        .await
        .context("failed to persist config")
}

/// Delete a skill bundle: archive its directory, strip it from every agent's
/// `skill_bundles` list, remove the config entry, and persist. Safe-by-default:
/// without `--yes` it prints the impact and makes no change.
async fn handle_bundle_remove(
    config: &crate::config::Config,
    alias: String,
    yes: bool,
) -> Result<()> {
    let exists = config
        .get_map_keys("skill_bundles")
        .is_some_and(|k| k.contains(&alias));
    if !exists {
        anyhow::bail!(
            "skill bundle '{alias}' is not configured",
            alias = alias.as_str()
        );
    }
    let refs = reiclaw_config::alias_refs::find_bundle_refs(config, &alias);
    if !yes {
        let count = refs.len().to_string();
        println!(
            "deleting skill_bundles.{alias} would strip it from {count} agent reference(s):",
            alias = alias.as_str(),
            count = count.as_str()
        );
        for r in &refs {
            println!("  • {}", r.path);
        }
        println!("\nNo changes made. Re-run with --yes to apply.");
        return Ok(());
    }
    let mut working = config.clone();
    let install_root = working.install_root_dir();
    // Resolve the bundle directory while the entry still exists, so it can be
    // archived AFTER the config change is durable.
    let bundle_dir =
        reiclaw_config::skill_bundles::resolve_directory(&working, &install_root, &alias)
            .ok()
            .filter(|d| d.exists());

    // Mutate + PERSIST the config first, so a later archive failure can't leave
    // the config pointing at a directory already moved to _deleted/.
    let mut dirty = reiclaw_config::alias_refs::scrub_bundle_refs(&mut working, &alias);
    working
        .delete_map_key("skill_bundles", &alias)
        .map_err(anyhow::Error::msg)?;
    dirty.push(format!("skill_bundles.{alias}"));
    for p in &dirty {
        working.mark_dirty(p);
    }
    Box::pin(working.save_dirty())
        .await
        .context("failed to persist config")?;

    // Archive the bundle directory under shared/skills/_deleted/ (the runtime
    // skips that path, so it isn't re-scanned as live skills) now that the
    // config change is on disk.
    if let Some(dir) = bundle_dir {
        let ts = jiff::Timestamp::now().strftime("%Y%m%d%H%M%S");
        let archive = install_root
            .join("shared")
            .join("skills")
            .join("_deleted")
            .join(format!("{alias}-{ts}"));
        if let Some(p) = archive.parent() {
            tokio::fs::create_dir_all(p).await.ok();
        }
        match tokio::fs::rename(&dir, &archive).await {
            Ok(()) => {
                let p = archive.display().to_string();
                println!("archived bundle directory → {path}", path = p.as_str());
            }
            Err(e) => {
                let es = e.to_string();
                eprintln!(
                    "warning: bundle directory archive failed: {error}",
                    error = es.as_str()
                );
            }
        }
    }
    let count = refs.len().to_string();
    println!(
        "deleted skill_bundles.{alias} (stripped from {count} agent(s))",
        alias = alias.as_str(),
        count = count.as_str()
    );
    Ok(())
}

/// Rename a skill bundle: rename the config entry, rewrite every agent's
/// `skill_bundles` reference, move its directory, and persist.
async fn handle_bundle_rename(
    config: &crate::config::Config,
    from: String,
    to: String,
) -> Result<()> {
    let mut working = config.clone();
    let install_root = working.install_root_dir();
    // Resolve the OLD directory while the `from` entry still exists.
    let old_dir =
        reiclaw_config::skill_bundles::resolve_directory(&working, &install_root, &from).ok();
    match working.rename_map_key("skill_bundles", &from, &to) {
        Ok(true) => {}
        Ok(false) => anyhow::bail!(
            "skill bundle '{alias}' is not configured",
            alias = from.as_str()
        ),
        Err(e) => {
            let es = e.to_string();
            anyhow::bail!("rename failed: {error}", error = es.as_str())
        }
    }
    let mut dirty = reiclaw_config::alias_refs::rewrite_bundle_refs(&mut working, &from, &to);
    dirty.push(format!("skill_bundles.{from}"));
    dirty.push(format!("skill_bundles.{to}"));
    // Resolve the NEW directory (the entry now lives under `to`) for the move.
    let new_dir =
        reiclaw_config::skill_bundles::resolve_directory(&working, &install_root, &to).ok();
    for p in &dirty {
        working.mark_dirty(p);
    }
    // PERSIST the config rename before moving the directory, so a later move
    // failure can't leave the config naming `to` while the dir sits at `from`.
    Box::pin(working.save_dirty())
        .await
        .context("failed to persist config")?;

    // Move the directory (default per-alias path only; a custom path is
    // alias-independent → old == new → skip).
    if let (Some(old), Some(new)) = (old_dir, new_dir)
        && old != new
        && old.exists()
    {
        if let Some(p) = new.parent() {
            tokio::fs::create_dir_all(p).await.ok();
        }
        if let Err(e) = tokio::fs::rename(&old, &new).await {
            let es = e.to_string();
            eprintln!(
                "warning: bundle directory move failed: {error}",
                error = es.as_str()
            );
        }
    }
    println!(
        "renamed skill_bundles.{from} → skill_bundles.{to}",
        from = from.as_str(),
        to = to.as_str()
    );
    Ok(())
}

fn print_bundle_include_exclude(include: &[String], exclude: &[String]) {
    if !include.is_empty() {
        println!("  include: {values}", values = include.join(", "));
    }
    if !exclude.is_empty() {
        println!("  exclude: {values}", values = exclude.join(", "));
    }
}

fn handle_bundle_list(config: &crate::config::Config) -> Result<()> {
    let install_root = config.install_root_dir();
    let service = SkillsService::new(config, install_root);
    let bundles = service.list_bundles()?;
    if bundles.is_empty() {
        println!(
            "No skill bundles configured.\n  Create one: rei config set skill-bundles.default.directory shared/skills/default"
        );
        return Ok(());
    }
    println!("Skill bundles ({count}):", count = bundles.len());
    for b in &bundles {
        println!(
            "  {alias} -> {dir}",
            alias = b.alias,
            dir = b.directory.display()
        );
        print_bundle_include_exclude(&b.include, &b.exclude);
    }
    Ok(())
}

fn handle_bundle_show(config: &crate::config::Config, alias: String) -> Result<()> {
    let install_root = config.install_root_dir();
    let service = SkillsService::new(config, install_root);
    let bundles = service.list_bundles()?;
    let bundle = bundles
        .into_iter()
        .find(|b| b.alias == alias)
        .ok_or_else(|| {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"skill_bundle": alias})),
                "skill bundle lookup failed: alias not in config"
            );
            anyhow::Error::msg(format!("skill bundle '{alias}' not configured"))
        })?;

    println!(
        "{alias} -> {dir}",
        alias = bundle.alias,
        dir = bundle.directory.display()
    );
    print_bundle_include_exclude(&bundle.include, &bundle.exclude);

    let skills = service.list_skills(Some(&alias))?;
    if skills.is_empty() {
        println!("  (no skills installed)");
    } else {
        println!("  skills ({count}):", count = skills.len());
        for s in &skills {
            println!(
                "    {name}: {description}",
                description = s.frontmatter.description,
                name = s.r#ref.name()
            );
        }
    }
    Ok(())
}

fn prompt_for_description(description: Option<String>) -> Result<String> {
    if let Some(d) = description
        && !d.trim().is_empty()
    {
        return Ok(d);
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let prompt: String = dialoguer::Input::new()
            .with_prompt("Skill description (what it does, when to use it)")
            .interact_text()?;
        if prompt.trim().is_empty() {
            anyhow::bail!("description must not be empty");
        }
        Ok(prompt)
    } else {
        anyhow::bail!("--description is required when stdin is not a TTY");
    }
}

fn open_in_editor(path: &std::path::Path) -> Result<()> {
    let Some(editor) = editor_from_env_or_path() else {
        anyhow::bail!("no editor found; set VISUAL or EDITOR");
    };
    let status = std::process::Command::new(&editor).arg(path).status()?;
    if !status.success() {
        anyhow::bail!("{editor} exited with non-zero status");
    }
    Ok(())
}

fn editor_from_env_or_path() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            fallback_editors()
                .iter()
                .copied()
                .find(|candidate| executable_on_path(candidate))
                .map(str::to_string)
        })
}

fn executable_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
}

fn fallback_editors() -> &'static [&'static str] {
    &["nano", "vi", "vim", "editor"]
}

#[cfg(test)]
mod install_location_tests {
    use super::*;
    use crate::config::{AliasedAgentConfig, Config};
    use reiclaw_config::schema::SkillBundleConfig;

    fn config_with_bundles(aliases: &[&str]) -> Config {
        let mut c = Config::default();
        for alias in aliases {
            c.skill_bundles
                .insert((*alias).to_string(), SkillBundleConfig::default());
        }
        c
    }

    fn agent_with_bundles(bundles: &[&str]) -> AliasedAgentConfig {
        AliasedAgentConfig {
            skill_bundles: bundles.iter().map(|s| (*s).to_string()).collect(),
            ..AliasedAgentConfig::default()
        }
    }

    #[test]
    fn explicit_bundle_wins() {
        let c = config_with_bundles(&["official"]);
        let loc = resolve_install_location(&c, None, Some("official")).unwrap();
        assert!(matches!(loc, SkillLocation::Bundle { alias, .. } if alias == "official"));
    }

    #[test]
    fn explicit_unknown_bundle_errors() {
        let c = config_with_bundles(&["official"]);
        assert!(resolve_install_location(&c, None, Some("ghost")).is_err());
    }

    #[test]
    fn unknown_agent_errors() {
        let c = config_with_bundles(&["official"]);
        assert!(resolve_install_location(&c, Some("nobody"), None).is_err());
    }

    #[test]
    fn no_agent_no_bundle_falls_back_to_global() {
        let c = config_with_bundles(&["official"]);
        let loc = resolve_install_location(&c, None, None).unwrap();
        assert!(matches!(loc, SkillLocation::Global { .. }));
    }

    #[test]
    fn default_agent_single_bundle_is_used() {
        let mut c = config_with_bundles(&["team"]);
        c.agents
            .insert("default".to_string(), agent_with_bundles(&["team"]));
        let loc = resolve_install_location(&c, None, None).unwrap();
        assert!(matches!(loc, SkillLocation::Bundle { alias, .. } if alias == "team"));
    }

    #[test]
    fn agent_with_multiple_bundles_requires_flag() {
        let mut c = config_with_bundles(&["a", "b"]);
        c.agents
            .insert("default".to_string(), agent_with_bundles(&["a", "b"]));
        assert!(resolve_install_location(&c, None, None).is_err());
    }

    #[test]
    fn explicit_agent_without_bundle_falls_back_to_global() {
        let mut c = Config::default();
        c.agents
            .insert("worker".to_string(), agent_with_bundles(&[]));
        let loc = resolve_install_location(&c, Some("worker"), None).unwrap();
        assert!(matches!(loc, SkillLocation::Global { .. }));
    }

    fn write_skill(dir: &Path, name: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.toml"),
            format!(
                "[skill]\nname = \"{name}\"\ndescription = \"boundary test skill\"\nversion = \"0.1.0\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn default_install_destination_is_loaded_by_the_runtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        let mut c = Config {
            // install_root_dir() == config_path.parent() == root
            config_path: root.join("config.toml"),
            data_dir: root.join("data"),
            ..Config::default()
        };
        c.skill_bundles
            .insert("official".to_string(), SkillBundleConfig::default());
        c.agents
            .insert("default".to_string(), agent_with_bundles(&["official"]));

        // Where `skills install` (no flags) would write for the default agent.
        let loc = resolve_install_location(&c, None, None).unwrap();
        let dest = match loc {
            SkillLocation::Bundle { ref alias, ref dir } => {
                assert_eq!(alias, "official");
                dir.clone()
            }
            SkillLocation::Global { .. } => panic!("expected the agent's bundle, got global"),
        };
        write_skill(&dest, "loadable-skill");

        // A skill left in the legacy global dir must NOT be loaded (the bug).
        write_skill(&skills_dir(&c.data_dir), "orphaned-skill");

        let loaded: Vec<String> = load_skills_for_agent_from_config(&c, "default")
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            loaded.iter().any(|n| n == "loadable-skill"),
            "install destination must be loaded by the runtime; got {loaded:?}"
        );
        assert!(
            !loaded.iter().any(|n| n == "orphaned-skill"),
            "data/skills must NOT be loaded by the runtime (this was #8334); got {loaded:?}"
        );
    }

    #[tokio::test]
    async fn install_command_then_runtime_loads_the_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        // A local skill source directory a user would `skills install`.
        let source_parent = root.join("source");
        write_skill(&source_parent, "e2e-skill");
        let source = source_parent.join("e2e-skill");

        let mut c = Config {
            // install_root_dir() == config_path.parent() == root
            config_path: root.join("config.toml"),
            data_dir: root.join("data"),
            ..Config::default()
        };
        c.skill_bundles
            .insert("official".to_string(), SkillBundleConfig::default());
        c.agents
            .insert("default".to_string(), agent_with_bundles(&["official"]));

        // Run the actual bin handler — no flags, so it resolves to the default
        // agent's single assigned bundle, exactly like `rei skills install`.
        handle_command(
            crate::SkillCommands::Install {
                source: source.to_string_lossy().into_owned(),
                agent: None,
                bundle: None,
                no_tier_banner: true,
                skill: None,
            },
            &c,
        )
        .await
        .expect("skills install should succeed for a local source");

        // The runtime loader must now see what install just wrote.
        let loaded: Vec<String> = load_skills_for_agent_from_config(&c, "default")
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            loaded.iter().any(|n| n == "e2e-skill"),
            "an installed skill must be loaded by the runtime; got {loaded:?}"
        );
    }
}
