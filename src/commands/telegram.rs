//! Compatibility diagnostics for retired in-process Telegram operator tooling.

use anyhow::Result;

/// Keep the legacy command parseable without writing markers that the
/// standalone Telegram bridge cannot consume.
pub fn run_skip_update(_alias: &str, _update_id: i64, _reason: Option<String>) -> Result<()> {
    anyhow::bail!("The in-process Telegram listener and telegram skip-update are retired. No skip marker was written. Telegram intake and cursor recovery belong to rei-bridge-telegram; this command cannot skip a bridge update.".to_string())
}
