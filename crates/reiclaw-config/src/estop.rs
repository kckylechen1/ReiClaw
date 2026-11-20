//! Local emergency-stop configuration, independent of model credentials.

use crate::schema::{Config, EstopConfig, OtpConfig, SecretsConfig, SecurityConfig};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// Only the settings consumed by the emergency-stop CLI. This value cannot be
/// saved as a full configuration or used to start inference capabilities.
#[derive(Debug)]
pub struct LocalEstopConfig {
    pub config_dir: PathBuf,
    pub estop: EstopConfig,
    pub otp: OtpConfig,
    pub secrets: SecretsConfig,
}

#[derive(Default, Deserialize)]
struct LocalSecurity {
    #[serde(default)]
    estop: EstopConfig,
    #[serde(default)]
    otp: OtpConfig,
}

impl LocalEstopConfig {
    /// Read local safety settings without initialization, secret resolution,
    /// schema salvage, or writes. Invalid safety values must never fall back to
    /// defaults, especially when deciding whether resume requires an OTP.
    pub async fn load() -> Result<Self> {
        let (config_dir, _) = crate::load::resolve_runtime_dirs().await?;
        let config_path = config_dir.join("config.toml");
        let contents = tokio::fs::read_to_string(&config_path)
            .await
            .with_context(|| {
                format!(
                    "Failed to read {} for emergency stop",
                    config_path.display()
                )
            })?;
        // TOML parse errors can echo unrelated secret values from the document.
        let raw: toml::Value = toml::from_str(&contents).map_err(|_| {
            let message = "Invalid config.toml syntax; emergency-stop settings were not loaded";
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject),
                message
            );
            anyhow::Error::msg(message)
        })?;

        // Check the generation using the shared gate, without making retired
        // providers/channels elsewhere in the file prerequisites for stopping.
        let mut generation = toml::map::Map::new();
        if let Some(version) = raw.get("schema_version") {
            generation.insert("schema_version".to_owned(), version.clone());
        }
        crate::current::require_current_schema(&toml::Value::Table(generation))?;

        let local_security: LocalSecurity = read_section(&raw, "security")?;
        let mut config = Config {
            security: SecurityConfig {
                estop: local_security.estop,
                otp: local_security.otp,
                ..SecurityConfig::default()
            },
            secrets: read_section(&raw, "secrets")?,
            ..Config::default()
        };
        // Reuse the normal parser and precedence for these exact local sections.
        // Ignore unrelated model/env settings without resolving their secrets.
        crate::env_overrides::apply_env_overrides_matching(&mut config, |tail| {
            tail == "security"
                || ["security__estop", "security__otp", "secrets"]
                    .iter()
                    .any(|section| {
                        tail == *section
                            || tail
                                .strip_prefix(*section)
                                .is_some_and(|rest| rest.starts_with("__"))
                    })
        })?;
        crate::validate::validate_estop_config(&config.security.otp, &config.security.estop)?;

        Ok(Self {
            config_dir,
            estop: config.security.estop,
            otp: config.security.otp,
            secrets: config.secrets,
        })
    }
}

fn read_section<T: for<'de> Deserialize<'de> + Default>(
    raw: &toml::Value,
    section: &str,
) -> Result<T> {
    match raw.get(section) {
        Some(value) => value.clone().try_into().map_err(|_| {
            let message = format!(
                "Invalid [{section}] settings; emergency-stop configuration was not loaded"
            );
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject),
                &message
            );
            anyhow::Error::msg(message)
        }),
        None => Ok(T::default()),
    }
}
