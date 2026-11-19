//! `rei soul` — Soul diagnostics.

use anyhow::Result;
use reiclaw_runtime::agent::soul_check::{
    ModelTarget, SCENARIOS, SoulCheckReport, planned_model_calls, run_soul_check,
};

use crate::config::Config;

/// Output format for the Soul check report.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table.
    Table,
    /// Machine-readable JSON.
    Json,
}

/// Run the Soul check for `agent`. Setup problems are errors; findings in the
/// report are not, so the exit status stays zero for them.
pub async fn check(
    config: &Config,
    agent: &str,
    targets: &[String],
    judge: Option<&str>,
) -> Result<SoulCheckReport> {
    let targets = targets
        .iter()
        .map(|spec| ModelTarget::parse(spec))
        .collect::<Result<Vec<_>>>()?;
    let judge = match judge {
        Some(spec) => ModelTarget::parse(spec)?,
        None => ModelTarget::agent_default(config, agent)?,
    };
    let target_count = targets.len().max(1);
    print_cost_notice(agent, target_count);
    run_soul_check(config, agent, &targets, judge).await
}

/// One line on stderr, so `--format json` keeps stdout machine-readable.
fn print_cost_notice(agent: &str, targets: usize) {
    let replies = targets * SCENARIOS.len();
    eprintln!(
        "Soul check for agent {agent}: this makes real model calls, up to {calls} ({replies} scenario replies plus {judgements} judge calls). Each selected target receives the agent's real Soul projection as its system prompt — only choose targets you want to send it to. Results are printed and not stored (a data directory without a seeded Soul gets one, same as the first chat turn).",
        calls = planned_model_calls(targets),
        judgements = replies,
        replies = replies
    );
}

/// Render a report in the requested format.
pub fn print_report(report: &SoulCheckReport, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", report.to_json()),
        OutputFormat::Table => println!("{}", report.render_table()),
    }
}
