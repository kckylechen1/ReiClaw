pub use reiclaw_runtime::cron::*;

use crate::config::Config;
use anyhow::{Context, Result, bail};
use reiclaw_runtime::i18n::{get_required_cli_string, get_required_cli_string_with_args};

/// Bail with a clear error if the named agent isn't configured.
fn require_configured_agent(config: &Config, agent_alias: &str) -> Result<()> {
    if config.agent(agent_alias).is_none() {
        ::reiclaw_log::record!(
            WARN,
            ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                .with_outcome(::reiclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"agent_alias": agent_alias})),
            "cron CLI rejected: unknown agent alias"
        );
        anyhow::bail!("Unknown agent {agent_alias:?} (no [agents.{agent_alias}] entry configured)");
    }
    Ok(())
}

fn parse_explicit_rfc3339_utc(raw: &str) -> Result<jiff::Timestamp> {
    reiclaw_api::time::parse_rfc3339(raw)
        .map_err(|err| {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "raw": raw,
                        "error": format!("{}", err),
                    })),
                "cron --at rejected: timestamp lacks explicit Z/offset or is malformed"
            );
            anyhow::Error::msg(format!(
                "Invalid RFC3339 timestamp for --at: expected RFC3339 timestamp with explicit Z or offset, e.g. 2026-05-18T09:00:00Z or 2026-05-18T09:00:00-04:00; got '{raw}': {err}"
            ))
        })
}

pub fn handle_command(command: crate::CronCommands, config: &Config) -> Result<()> {
    match command {
        crate::CronCommands::List => {
            let jobs = list_jobs(config)?;
            if jobs.is_empty() {
                println!("{}", get_required_cli_string("cli-cron-none"));
                println!("\n{}", get_required_cli_string("cli-cron-usage"));
                println!("  rei cron add '0 9 * * *' 'agent -m \"Good morning!\"'"); // i18n-exempt: literal command example
                return Ok(());
            }

            println!(
                "{}",
                get_required_cli_string_with_args(
                    "cli-cron-jobs-header",
                    &[("count", &jobs.len().to_string())]
                )
            );
            for job in jobs {
                let last_run = job
                    .last_run
                    .map_or_else(|| "never".into(), reiclaw_api::time::format_rfc3339);
                let last_status = job.last_status.unwrap_or_else(|| "n/a".into());
                println!(
                    "- {} | {:?} | next={} | last={} ({})",
                    job.id,
                    job.schedule,
                    reiclaw_api::time::format_rfc3339(job.next_run),
                    last_run,
                    last_status,
                );
                if !job.command.is_empty() {
                    println!(
                        "{}",
                        get_required_cli_string_with_args(
                            "cli-cron-list-cmd",
                            &[("cmd", &job.command)]
                        )
                    );
                }
                if let Some(prompt) = &job.prompt {
                    println!(
                        "{}",
                        get_required_cli_string_with_args(
                            "cli-cron-list-prompt",
                            &[("prompt", prompt)]
                        )
                    );
                }
            }
            Ok(())
        }
        crate::CronCommands::Add {
            expression,
            agent_alias,
            tz,
            prompt,
            allowed_tools,
            uses_memory,
            command,
        } => {
            require_configured_agent(config, &agent_alias)?;
            let schedule = Schedule::Cron {
                expr: expression,
                tz,
            };
            if prompt {
                let job = add_agent_job(
                    config,
                    &agent_alias,
                    None,
                    schedule,
                    &command,
                    SessionTarget::Isolated,
                    None,
                    None,
                    false,
                    if allowed_tools.is_empty() {
                        None
                    } else {
                        Some(allowed_tools)
                    },
                    uses_memory.unwrap_or(true),
                )?;
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-added-agent", &[("id", &job.id)])
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-expr", &[("v", &job.expression)])
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-next",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-prompt",
                        &[("v", job.prompt.as_deref().unwrap_or_default())]
                    )
                );
            } else {
                if !allowed_tools.is_empty() {
                    bail!("--allowed-tool is only supported with --prompt cron jobs");
                }
                let job = add_shell_job(config, &agent_alias, None, schedule, &command)?;
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-added", &[("id", &job.id)])
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-expr2", &[("v", &job.expression)])
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-next2",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-cmd", &[("v", &job.command)])
                );
            }
            Ok(())
        }
        crate::CronCommands::AddAt {
            at,
            agent_alias,
            prompt,
            allowed_tools,
            uses_memory,
            command,
        } => {
            require_configured_agent(config, &agent_alias)?;
            let at = parse_explicit_rfc3339_utc(&at)?;
            let schedule = Schedule::At { at };
            if prompt {
                let job = add_agent_job(
                    config,
                    &agent_alias,
                    None,
                    schedule,
                    &command,
                    SessionTarget::Isolated,
                    None,
                    None,
                    true,
                    if allowed_tools.is_empty() {
                        None
                    } else {
                        Some(allowed_tools)
                    },
                    uses_memory.unwrap_or(true),
                )?;
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-added-oneshot-agent",
                        &[("id", &job.id)]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-at",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-prompt",
                        &[("v", job.prompt.as_deref().unwrap_or_default())]
                    )
                );
            } else {
                if !allowed_tools.is_empty() {
                    bail!("--allowed-tool is only supported with --prompt cron jobs");
                }
                let job = add_shell_job(config, &agent_alias, None, schedule, &command)?;
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-added-oneshot", &[("id", &job.id)])
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-at2",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-cmd", &[("v", &job.command)])
                );
            }
            Ok(())
        }
        crate::CronCommands::AddEvery {
            every_ms,
            agent_alias,
            prompt,
            allowed_tools,
            uses_memory,
            command,
        } => {
            require_configured_agent(config, &agent_alias)?;
            let schedule = Schedule::Every { every_ms };
            if prompt {
                let job = add_agent_job(
                    config,
                    &agent_alias,
                    None,
                    schedule,
                    &command,
                    SessionTarget::Isolated,
                    None,
                    None,
                    false,
                    if allowed_tools.is_empty() {
                        None
                    } else {
                        Some(allowed_tools)
                    },
                    uses_memory.unwrap_or(true),
                )?;
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-added-interval-agent",
                        &[("id", &job.id)]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-every",
                        &[("v", &every_ms.to_string())]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-next3",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-prompt3",
                        &[("v", job.prompt.as_deref().unwrap_or_default())]
                    )
                );
            } else {
                if !allowed_tools.is_empty() {
                    bail!("--allowed-tool is only supported with --prompt cron jobs");
                }
                let job = add_shell_job(config, &agent_alias, None, schedule, &command)?;
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-added-interval",
                        &[("id", &job.id)]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-every",
                        &[("v", &every_ms.to_string())]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-next3",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-cmd3", &[("v", &job.command)])
                );
            }
            Ok(())
        }
        crate::CronCommands::Once {
            delay,
            agent_alias,
            prompt,
            allowed_tools,
            uses_memory,
            command,
        } => {
            require_configured_agent(config, &agent_alias)?;
            if prompt {
                let duration = parse_delay(&delay)?;
                let at = jiff::Timestamp::now()
                    .checked_add(duration)
                    .context("Cron delay exceeds the supported timestamp range")?;
                let schedule = Schedule::At { at };
                let job = add_agent_job(
                    config,
                    &agent_alias,
                    None,
                    schedule,
                    &command,
                    SessionTarget::Isolated,
                    None,
                    None,
                    true,
                    if allowed_tools.is_empty() {
                        None
                    } else {
                        Some(allowed_tools)
                    },
                    uses_memory.unwrap_or(true),
                )?;
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-added-oneshot-agent",
                        &[("id", &job.id)]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-at",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-prompt",
                        &[("v", job.prompt.as_deref().unwrap_or_default())]
                    )
                );
            } else {
                if !allowed_tools.is_empty() {
                    bail!("--allowed-tool is only supported with --prompt cron jobs");
                }
                let job = add_once(config, &agent_alias, &delay, &command)?;
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-added-oneshot", &[("id", &job.id)])
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args(
                        "cli-cron-at2",
                        &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                    )
                );
                println!(
                    "{}",
                    get_required_cli_string_with_args("cli-cron-cmd", &[("v", &job.command)])
                );
            }
            Ok(())
        }
        crate::CronCommands::Update {
            id,
            agent_alias,
            expression,
            tz,
            command,
            name,
            allowed_tools,
            uses_memory,
        } => {
            require_configured_agent(config, &agent_alias)?;
            if expression.is_none()
                && tz.is_none()
                && command.is_none()
                && name.is_none()
                && allowed_tools.is_empty()
                && uses_memory.is_none()
            {
                bail!("{}", get_required_cli_string("cli-cron-update-no-field"));
            }

            let existing = if expression.is_some() || tz.is_some() || !allowed_tools.is_empty() {
                Some(get_job(config, &id)?)
            } else {
                None
            };

            // Merge expression/tz with the existing schedule so that
            // --tz alone updates the timezone and --expression alone
            // preserves the existing timezone.
            let schedule = if expression.is_some() || tz.is_some() {
                let existing = existing
                    .as_ref()
                    .expect("existing job must be loaded when updating schedule");
                let (existing_expr, existing_tz) = match &existing.schedule {
                    Schedule::Cron {
                        expr,
                        tz: existing_tz,
                    } => (expr.clone(), existing_tz.clone()),
                    _ => bail!("Cannot update expression/tz on a non-cron schedule"),
                };
                Some(Schedule::Cron {
                    expr: expression.unwrap_or(existing_expr),
                    tz: tz.or(existing_tz),
                })
            } else {
                None
            };

            if !allowed_tools.is_empty() {
                let existing = existing
                    .as_ref()
                    .expect("existing job must be loaded when updating allowed tools");
                if existing.job_type != JobType::Agent {
                    bail!("--allowed-tool is only supported for agent cron jobs");
                }
            }

            let patch = CronJobPatch {
                schedule,
                command,
                name,
                allowed_tools: if allowed_tools.is_empty() {
                    None
                } else {
                    Some(allowed_tools)
                },
                uses_memory,
                ..CronJobPatch::default()
            };

            let job = update_shell_job_with_approval(config, &agent_alias, &id, patch, false)?;
            println!(
                "{}",
                get_required_cli_string_with_args("cli-cron-updated", &[("id", &job.id)])
            );
            println!(
                "{}",
                get_required_cli_string_with_args("cli-cron-expr2", &[("v", &job.expression)])
            );
            println!(
                "{}",
                get_required_cli_string_with_args(
                    "cli-cron-next2",
                    &[("v", &reiclaw_api::time::format_rfc3339(job.next_run))]
                )
            );
            println!(
                "{}",
                get_required_cli_string_with_args("cli-cron-cmd", &[("v", &job.command)])
            );
            Ok(())
        }
        crate::CronCommands::Remove { id } => {
            remove_job(config, &id)?;
            println!(
                "{}",
                get_required_cli_string_with_args("cli-cron-removed", &[("id", &id)])
            );
            Ok(())
        }
        crate::CronCommands::Pause { id } => {
            pause_job(config, &id)?;
            println!(
                "{}",
                get_required_cli_string_with_args("cli-cron-paused", &[("id", &id)])
            );
            Ok(())
        }
        crate::CronCommands::Resume { id } => {
            resume_job(config, &id)?;
            println!(
                "{}",
                get_required_cli_string_with_args("cli-cron-resumed", &[("id", &id)])
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let mut config = Config {
            data_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        config
            .risk_profiles
            .entry("test-agent".to_string())
            .or_default();
        config
            .runtime_profiles
            .entry("test-agent".to_string())
            .or_default();
        config
            .providers
            .models
            .ensure("openai", "test-agent")
            .expect("known family");
        config.agents.entry("test-agent".to_string()).or_insert(
            reiclaw_config::schema::AliasedAgentConfig {
                model_provider: "openai.test-agent".into(),
                risk_profile: "test-agent".into(),
                runtime_profile: "test-agent".into(),
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn cli_add_at_rejects_timestamp_without_explicit_offset_with_actionable_error() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let result = handle_command(
            crate::CronCommands::AddAt {
                at: "2026-05-18T09:00:00".into(),
                agent_alias: "test-agent".into(),
                prompt: false,
                allowed_tools: vec![],
                uses_memory: None,
                command: "echo at".into(),
            },
            &config,
        );

        let error = result.expect_err("bare local timestamp must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("RFC3339 timestamp with explicit Z or offset"),
            "error should explain the explicit offset requirement: {message}"
        );
        assert!(message.contains("2026-05-18T09:00:00Z"));
        assert!(message.contains("2026-05-18T09:00:00-04:00"));
    }

    #[test]
    fn cli_once_prompt_rejects_unrepresentable_delay_without_creating_a_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let error = handle_command(
            crate::CronCommands::Once {
                delay: "3000000d".into(),
                agent_alias: "test-agent".into(),
                prompt: true,
                allowed_tools: vec![],
                uses_memory: None,
                command: "send a reminder".into(),
            },
            &config,
        )
        .expect_err("a valid duration outside the timestamp range must be rejected");

        assert!(error.to_string().contains("supported timestamp range"));
        assert!(list_jobs(&config).unwrap().is_empty());
    }
}
