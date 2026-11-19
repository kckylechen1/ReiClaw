//! `rei gateway bridge add|remove|list`: operator management of
//! `[gateway.bridges.<name>]`. A bridge token is minted here and printed
//! once; only its SHA-256 hash is written to the config.

use anyhow::{Result, bail};

use crate::BridgeCommands;
use crate::config::Config;
use reiclaw_config::schema::GatewayBridgeConfig;
use reiclaw_runtime::security::pairing::{PairingGuard, generate_bridge_token};

pub async fn handle_bridge_command(config: &mut Config, command: BridgeCommands) -> Result<()> {
    match command {
        BridgeCommands::Add {
            name,
            sessions,
            session_prefix,
            rotate,
        } => add(config, &name, sessions, session_prefix, rotate).await,
        BridgeCommands::Remove { name } => remove(config, &name).await,
        BridgeCommands::List => {
            list(config);
            Ok(())
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn scope_text(bridge: &GatewayBridgeConfig) -> String {
    let mut parts: Vec<String> = bridge.sessions.clone();
    if let Some(prefix) = bridge.session_prefix.as_deref().filter(|p| !p.is_empty()) {
        parts.push(format!("{prefix}*"));
    }
    parts.join(", ")
}

async fn add(
    config: &mut Config,
    name: &str,
    sessions: Vec<String>,
    session_prefix: Option<String>,
    rotate: bool,
) -> Result<()> {
    if !valid_name(name) {
        bail!(format!(
            "Invalid bridge name \"{name}\": use letters, digits, '-' and '_'."
        ));
    }
    let existing = config.gateway.bridges.get(name).cloned();
    if existing.is_some() && !rotate {
        bail!(format!(
            "Bridge \"{name}\" already exists; pass --rotate to replace its token."
        ));
    }
    if existing.is_none() && rotate {
        bail!(format!("No bridge named \"{name}\" is configured."));
    }
    crate::config::current::ensure_disk_at_current_version(&config.config_path)?;

    let token = generate_bridge_token();
    let mut bridge = existing.unwrap_or_default();
    bridge.token_hash = PairingGuard::token_hash(&token);
    if !sessions.is_empty() {
        bridge.sessions = sessions;
    }
    if session_prefix.is_some() {
        bridge.session_prefix = session_prefix.filter(|p| !p.is_empty());
    }
    let scope = scope_text(&bridge);
    config.gateway.bridges.insert(name.to_string(), bridge);
    for field in ["token_hash", "sessions", "session_prefix"] {
        config.mark_dirty(&format!("gateway.bridges.{name}.{field}"));
    }
    Box::pin(config.save_dirty()).await?;

    println!("Bridge \"{name}\" saved in [gateway.bridges.{name}].");
    println!();
    println!("Bridge token (shown only once; give it to the bridge, e.g. as REI_GATEWAY_TOKEN):");
    println!("  {token}");
    println!();
    if scope.is_empty() {
        println!(
            "This token may open no chat session; add --session or --session-prefix to allow some."
        );
    } else {
        println!("Chat sessions this token may open: {scope}");
    }
    println!("Restart the gateway (or POST /admin/reload) to apply.");
    Ok(())
}

async fn remove(config: &mut Config, name: &str) -> Result<()> {
    if config.gateway.bridges.remove(name).is_none() {
        bail!(format!("No bridge named \"{name}\" is configured."));
    }
    crate::config::current::ensure_disk_at_current_version(&config.config_path)?;
    config.mark_dirty(&format!("gateway.bridges.{name}"));
    Box::pin(config.save_dirty()).await?;
    println!("Bridge \"{name}\" removed; its token no longer works.");
    println!("Restart the gateway (or POST /admin/reload) to apply.");
    Ok(())
}

fn list(config: &Config) {
    if config.gateway.bridges.is_empty() {
        println!("No bridges configured. Add one with: rei gateway bridge add <name>");
        return;
    }
    let mut names: Vec<&String> = config.gateway.bridges.keys().collect();
    names.sort();
    for name in names {
        let scope = scope_text(&config.gateway.bridges[name]);
        let scope = if scope.is_empty() {
            "(none)".to_string()
        } else {
            scope
        };
        println!("{name}: sessions {scope}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_names_are_config_keys() {
        assert!(valid_name("telegram"));
        assert!(valid_name("tg-2_b"));
        assert!(!valid_name(""));
        assert!(!valid_name("a.b"));
        assert!(!valid_name("a b"));
    }

    #[tokio::test]
    async fn add_writes_only_the_hash_and_remove_revokes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut config = Config {
            config_path: path.clone(),
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        Box::pin(config.save()).await.unwrap();

        add(&mut config, "telegram", vec!["main".into()], None, false)
            .await
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let bridge = config.gateway.bridges["telegram"].clone();
        assert!(written.contains("[gateway.bridges.telegram]"), "{written}");
        assert!(written.contains(&bridge.token_hash) || written.contains("enc"));
        assert!(
            !written.contains("zcb_"),
            "the token itself is never stored"
        );
        assert_eq!(bridge.sessions, ["main"]);

        // A second add needs --rotate, which keeps the scope.
        assert!(
            add(&mut config, "telegram", vec![], None, false)
                .await
                .is_err()
        );
        add(&mut config, "telegram", vec![], Some("tg:".into()), true)
            .await
            .unwrap();
        let rotated = &config.gateway.bridges["telegram"];
        assert_ne!(rotated.token_hash, bridge.token_hash);
        assert_eq!(rotated.sessions, ["main"]);
        assert_eq!(rotated.session_prefix.as_deref(), Some("tg:"));

        remove(&mut config, "telegram").await.unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("gateway.bridges.telegram"), "{written}");
        assert!(!written.contains("token_hash"), "{written}");
        assert!(remove(&mut config, "telegram").await.is_err());
    }
}
