pub use reiclaw_runtime::migration::*;

use crate::config::Config;
use anyhow::Result;

pub async fn handle_command(command: crate::MigrateCommands, config: &Config) -> Result<()> {
    match command {
        crate::MigrateCommands::Openclaw {
            source,
            dry_run,
            reindex,
        } => migrate_openclaw_memory(config, source, dry_run, reindex).await,
    }
}
