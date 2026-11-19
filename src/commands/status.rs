//! `rei status` — show system status.

use anyhow::Result;

use crate::config::Config;
use crate::cost;
use crate::print_companion_outbox_line;
use crate::service;

/// Print full system status, or exit 0/1 for Docker HEALTHCHECK (`--format exit-code`).
pub async fn handle(config: &Config, format: Option<String>) -> Result<()> {
    if format.as_deref() == Some("exit-code") {
        // Lightweight health probe for Docker HEALTHCHECK
        let port = config.gateway.port;
        let host = if config.gateway.host == "[::]" || config.gateway.host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            &config.gateway.host
        };
        let url = format!("http://{}:{}/health", host, port);
        match reqwest::Client::new()
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                std::process::exit(0);
            }
            _ => {
                std::process::exit(1);
            }
        }
    }
    println!("🦀 ReiClaw Status");
    println!();
    println!("Version:     {v}", v = env!("CARGO_PKG_VERSION"));
    println!("Workspace:   {v}", v = config.data_dir.display());
    println!("Config:      {v}", v = config.config_path.display());
    println!();
    let mut shown_provider = false;
    for (family, alias, entry) in config.providers.models.iter_entries() {
        let model = entry.model.as_deref().unwrap_or("(none)");
        if shown_provider {
            println!("   ModelProvider:      {family}.{alias}");
            println!("   Model:         {model}");
        } else {
            println!("🤖 ModelProvider:      {family}.{alias}");
            println!("   Model:         {model}");
            shown_provider = true;
        }
    }
    if !shown_provider {
        println!("🤖 ModelProvider:      (none configured)");
    }
    println!(
        "📊 Observability:  {v}",
        v = config.observability.backend.as_wire()
    );
    let trace_storage_mode = config.observability.log_persistence.as_wire().to_string();
    let trace_storage_path = config.observability.log_persistence_path.to_string();
    println!("🧾 Trace storage:  {trace_storage_mode} ({trace_storage_path})");
    // Per-agent autonomy: each enabled agent picks its own
    // risk_profile, so list them rather than collapsing to one.
    let mut agent_aliases: Vec<&String> = config
        .agents
        .iter()
        .filter(|(_, a)| a.enabled)
        .map(|(alias, _)| alias)
        .collect();
    agent_aliases.sort();
    if agent_aliases.is_empty() {
        println!("🛡️  Agents:        (none configured)");
    } else {
        let summary: Vec<String> = agent_aliases
            .iter()
            .map(|alias| match config.risk_profile_for_agent(alias) {
                Some(p) => format!("{alias}={:?}", p.level),
                None => format!("{alias}=<no risk_profile>"),
            })
            .collect();
        println!("🛡️  Agents:        {v}", v = summary.join(", "));
    }
    println!("⚙️  Runtime:       {v}", v = config.runtime.kind.as_wire());
    if service::is_running(config) {
        println!("🟢 Service:       running");
    } else {
        println!("🔴 Service:       stopped");
    }
    let effective_memory_backend = self::effective_memory_backend(config);
    let heartbeat_value = if config.heartbeat.enabled {
        let interval_minutes = config.heartbeat.interval_minutes.to_string();
        format!("every {interval_minutes}min")
    } else {
        "disabled".to_string()
    };
    println!("💓 Heartbeat:      {heartbeat_value}");
    let memory_backend = effective_memory_backend.to_string();
    let memory_auto_save = if config.memory.auto_save {
        "on".to_string()
    } else {
        "off".to_string()
    };
    println!("🧠 Memory:         {memory_backend} (auto-save: {memory_auto_save})");
    print_companion_outbox_line(config);

    println!();
    // Per-agent security: each enabled agent's risk profile.
    for alias in &agent_aliases {
        let Some(profile) = config.risk_profile_for_agent(alias) else {
            println!("Security ({alias}): <no risk_profile>");
            continue;
        };
        println!("Security ({alias}):");
        println!("  Workspace only:    {v}", v = profile.workspace_only);
        let allowed_roots = if profile.allowed_roots.is_empty() {
            "(none)".to_string()
        } else {
            profile.allowed_roots.join(", ")
        };
        println!("  Allowed roots:     {allowed_roots}");
        let allowed_commands = profile.allowed_commands.join(", ");
        println!("  Allowed commands:  {allowed_commands}");
        let actions_cap = config
            .runtime_profile_for_agent(alias)
            .map_or(0, |r| r.max_actions_per_hour);
        println!("  Max actions/hour:  {v}", v = actions_cap);
    }
    let cost_tracking = if config.cost.enabled {
        "enabled".to_string()
    } else {
        "disabled".to_string()
    };
    println!("  Cost tracking:     {cost_tracking}");
    println!("  Max cost/day:      ${:.2}", config.cost.daily_limit_usd);
    println!("  Max cost/month:    ${:.2}", config.cost.monthly_limit_usd);
    if config.cost.enabled {
        match cost::CostTracker::new(config.cost.clone(), &config.data_dir) {
            Ok(tracker) => match tracker.get_summary() {
                Ok(summary) => {
                    let spent_today = format!("{:.4}", summary.daily_cost_usd);
                    let daily_limit = format!("{:.2}", config.cost.daily_limit_usd);
                    println!("  Spent today:       ${spent_today} / ${daily_limit}");
                    let spent_month = format!("{:.4}", summary.monthly_cost_usd);
                    let monthly_limit = format!("{:.2}", config.cost.monthly_limit_usd);
                    println!("  Spent this month:  ${spent_month} / ${monthly_limit}");
                }
                Err(e) => {
                    eprintln!("  ⚠ Could not load cost usage: {err}", err = e);
                }
            },
            Err(e) => {
                eprintln!("  ⚠ Could not init cost tracker: {err}", err = e);
            }
        }
    }
    println!("  OTP enabled:       {v}", v = config.security.otp.enabled);
    println!(
        "  E-stop enabled:    {v}",
        v = config.security.estop.enabled
    );
    println!();
    println!("Channels:");
    println!("  CLI:      ✅ always");
    for entry in crate::channels::listing::compiled_channels(&config.channels) {
        let channel_status = if entry.configured {
            "configured".to_string()
        } else {
            "not configured".to_string()
        };
        println!(
            "  {:9} {}",
            entry.name,
            if entry.configured {
                format!("✅ {}", channel_status)
            } else {
                format!("❌ {}", channel_status)
            }
        );
    }
    let uncompiled = crate::channels::listing::configured_uncompiled_channels(&config.channels);
    if !uncompiled.is_empty() {
        println!("  Configured but not compiled in this binary:");
        for entry in &uncompiled {
            println!("  {:9} 🚫 configured, not compiled", entry.name);
        }
        println!(
            "  Build from source with `./install.sh --source --preset full`, `--features channels-full`, or the specific `channel-*` feature."
        );
    }
    let retired = crate::channels::listing::configured_retired_channels(&config.channels);
    if !retired.is_empty() {
        println!("  Configured sections of retired transports (no runner exists):");
        for entry in &retired {
            println!(
                "  {:9} ⚠️ section parses, nothing runs; remove it",
                entry.name
            );
        }
        println!("  Live surfaces: Telegram bridge, webhook ingress.");
    }

    Ok(())
}

/// Resolve the memory backend label printed by `rei status`.
///
/// Runtime construction reads `config.memory.backend` directly (`memorycore`,
/// `markdown`, `none`); `Config::resolve_active_storage` returns
/// `ActiveStorage::None` for `memorycore` and alias-less `markdown` because only
/// explicit `[storage.markdown.<alias>]` entries appear under `config.storage`.
fn effective_memory_backend(config: &Config) -> &'static str {
    let backend = crate::memory::backend_kind_from_dotted(&config.memory.backend);
    if backend.is_empty() {
        return "none";
    }
    match crate::memory::classify_memory_backend(&backend) {
        #[cfg(feature = "memory-core")]
        crate::memory::MemoryBackendKind::MemoryCore => "memorycore",
        crate::memory::MemoryBackendKind::Markdown => "markdown",
        crate::memory::MemoryBackendKind::None => "none",
        crate::memory::MemoryBackendKind::Retired(name) => name,
        crate::memory::MemoryBackendKind::Unknown => {
            if backend == "memorycore" {
                "memorycore"
            } else {
                "markdown"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::effective_memory_backend;
    use crate::config::Config;

    #[test]
    fn default_config_reports_memorycore_not_none() {
        let config = Config::default();
        assert_eq!(effective_memory_backend(&config), "memorycore");
    }

    #[test]
    fn markdown_and_dotted_markdown_report_markdown_without_storage_alias() {
        for raw in ["markdown", "markdown.notes", " MARKDOWN "] {
            let mut config = Config::default();
            config.memory.backend = raw.to_string();
            assert_eq!(effective_memory_backend(&config), "markdown");
        }
    }

    #[test]
    fn none_and_empty_report_none() {
        for raw in ["none", "", "   "] {
            let mut config = Config::default();
            config.memory.backend = raw.to_string();
            assert_eq!(effective_memory_backend(&config), "none");
        }
    }

    #[test]
    fn retired_backend_preserves_name_instead_of_masking_as_none() {
        let mut config = Config::default();
        config.memory.backend = "sqlite".to_string();
        assert_eq!(effective_memory_backend(&config), "sqlite");
    }
}
