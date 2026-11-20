//! Raw admission checks for retired settings that carry credentials or policy.
//!
//! Only empty sections and known serialized defaults may be discarded with a
//! warning. Configured credentials, policy changes, unknown fields and malformed
//! values require an explicit operator edit before parsing or persistence.

use anyhow::Result;
use toml::Value;

const SURFACES: &[&str] = &[
    "security.nevis",
    "security.webauthn",
    "node_transport",
    "tunnel",
];

/// A pre-write refusal that never includes submitted values or field names.
#[derive(Debug)]
pub struct RetiredSecurityConfigError(&'static str);

impl std::fmt::Display for RetiredSecurityConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == "tunnel" {
            return formatter.write_str(
                "[tunnel] is retired. Run tunnels and VPNs as host-managed services, then \
                 explicitly remove [tunnel] and its REI_ overrides. Existing credentials, \
                 commands and network policy are not migrated or silently discarded.",
            );
        }
        write!(
            formatter,
            "[{}] is retired and has no runtime enforcement. Review and remove its \
             non-default, unknown or malformed settings and corresponding REI_ overrides \
             explicitly; credentials and security policy are not migrated.",
            self.0
        )
    }
}

impl std::error::Error for RetiredSecurityConfigError {}

enum HarmlessDefault {
    Bool(bool),
    Integer(i64),
    String(&'static str),
    EmptyArray,
    EmptyTable,
}

impl HarmlessDefault {
    fn matches(&self, value: &Value) -> bool {
        match self {
            Self::Bool(expected) => value.as_bool() == Some(*expected),
            Self::Integer(expected) => value.as_integer() == Some(*expected),
            Self::String(expected) => value
                .as_str()
                .is_some_and(|actual| actual.is_empty() || actual == *expected),
            Self::EmptyArray => value.as_array().is_some_and(Vec::is_empty),
            Self::EmptyTable => value.as_table().is_some_and(toml::Table::is_empty),
        }
    }

    fn matches_env(&self, raw: &str) -> bool {
        // String overrides normally contain the unquoted value. Also accept
        // an explicitly quoted empty/default TOML string, never another type.
        if let Self::String(expected) = self
            && (raw.is_empty() || raw == *expected)
        {
            return true;
        }
        parse_env_literal(raw).is_some_and(|value| self.matches(&value))
    }
}

fn harmless_default(surface: &str, field: &str) -> Option<HarmlessDefault> {
    use HarmlessDefault::{Bool, EmptyArray, EmptyTable, Integer, String};
    match (surface, field) {
        ("security.nevis", "enabled" | "require_mfa")
        | ("security.webauthn", "enabled")
        | ("node_transport", "mutual_tls") => Some(Bool(false)),
        ("security.nevis", "instance_url" | "client_id" | "client_secret" | "jwks_url")
        | ("node_transport", "shared_secret" | "tls_cert_path" | "tls_key_path") => {
            Some(String(""))
        }
        ("security.nevis", "realm") => Some(String("master")),
        ("security.nevis", "token_validation") => Some(String("local")),
        ("security.nevis", "session_timeout_secs") => Some(Integer(3600)),
        ("security.nevis", "role_mapping") | ("node_transport", "allowed_peers") => {
            Some(EmptyArray)
        }
        ("security.webauthn", "rp_id") => Some(String("localhost")),
        ("security.webauthn", "rp_origin") => Some(String("http://localhost:42617")),
        ("security.webauthn", "rp_name") => Some(String("ReiClaw")),
        ("node_transport", "enabled" | "require_https") => Some(Bool(true)),
        ("node_transport", "max_request_age_secs") => Some(Integer(300)),
        ("node_transport", "connection_pool_size") => Some(Integer(4)),
        ("tunnel", "tunnel_provider") => Some(String("none")),
        // Disabled serialized defaults contain no provider blocks. Empty
        // blocks carry no setting; any provider field requires migration,
        // even with `tunnel_provider = "none"` (credentials can be dormant).
        ("tunnel", "cloudflare" | "tailscale" | "ngrok" | "openvpn" | "pinggy" | "custom") => {
            Some(EmptyTable)
        }
        _ => None,
    }
}

fn reject_configured_section(surface: &'static str, value: &Value) -> Result<()> {
    let harmless = value.as_table().is_some_and(|table| {
        table.iter().all(|(field, value)| {
            harmless_default(surface, field).is_some_and(|default| default.matches(value))
        })
    });
    if !harmless {
        return Err(RetiredSecurityConfigError(surface).into());
    }
    Ok(())
}

pub(crate) fn reject_retired_security_config(root: &Value) -> Result<()> {
    for &surface in SURFACES {
        let value = surface
            .split('.')
            .try_fold(root, |value, key| value.get(key));
        if let Some(value) = value {
            reject_configured_section(surface, value)?;
        }
    }
    Ok(())
}

/// The file loader collects warnings before strict admission. Do not describe
/// configured security policy as ignored when admission will refuse it.
pub(crate) fn permits_section_warning(path: &str, root: &Value) -> bool {
    let Some(&surface) = SURFACES.iter().find(|&&surface| surface == path) else {
        return true;
    };
    surface
        .split('.')
        .try_fold(root, |value, key| value.get(key))
        .is_none_or(|value| reject_configured_section(surface, value).is_ok())
}

/// Check the retired override before the generic warning-only tombstone.
pub(crate) fn reject_retired_security_env(tail: &str, raw: &str) -> Result<()> {
    for &surface in SURFACES {
        let prefix = surface.replace('.', "__");
        if tail == prefix {
            let value = parse_env_literal(raw).ok_or(RetiredSecurityConfigError(surface))?;
            return reject_configured_section(surface, &value);
        }
        if let Some(field) = tail.strip_prefix(&format!("{prefix}__")) {
            if !harmless_default(surface, field).is_some_and(|default| default.matches_env(raw)) {
                return Err(RetiredSecurityConfigError(surface).into());
            }
            return Ok(());
        }
    }
    Ok(())
}

fn parse_env_literal(raw: &str) -> Option<Value> {
    // Parse errors can quote secrets, so discard them at this boundary. Require
    // a single value: an appended TOML assignment is not a harmless literal.
    let mut table: toml::Table = toml::from_str(&format!("value = {raw}")).ok()?;
    if table.len() != 1 {
        return None;
    }
    table.remove("value")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::current::{parse_current, parse_current_resilient};

    #[test]
    fn retired_tunnel_disabled_defaults_warn_without_exposing_a_live_surface() {
        let config = crate::schema::Config::default();
        assert!(
            config
                .prop_schema()
                .iter()
                .all(|field| !field.name.starts_with("tunnel."))
        );
        assert!(crate::sections::Section::from_key("tunnel").is_none());
        assert!(
            toml::Value::try_from(config)
                .unwrap()
                .get("tunnel")
                .is_none()
        );
        for suffix in [
            "[tunnel]\n",
            "[tunnel]\ntunnel_provider = 'none'\n",
            "[tunnel.cloudflare]\n",
        ] {
            let raw = format!("schema_version = 3\n{suffix}");
            parse_current(&raw).unwrap();
            parse_current_resilient(&raw).unwrap();
            let warnings = crate::validation_warnings::retired_section_tombstones(&raw);
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].path, "tunnel");
            assert_eq!(warnings[0].code, "in_process_tunnel_removed");
        }
    }

    #[test]
    fn retired_tunnel_file_refuses_active_credentials_commands_and_malformed_settings() {
        for settings in [
            "tunnel_provider = 'cloudflare'",
            "tunnel_provider = 'none'\nngrok = { auth_token = 'private-sentinel' }",
            "cloudflare = { token = 'enc:private-sentinel' }",
            "tailscale = { funnel = true }",
            "openvpn = { config_file = 'private-sentinel', auth_file = 'private-sentinel' }",
            "pinggy = { token = 'private-sentinel' }",
            "custom = { start_command = 'private-sentinel' }",
            "tunnel_provider = false",
            "cloudflare = 'private-sentinel'",
            "future_policy = 'private-sentinel'",
        ] {
            let raw = format!("schema_version = 3\n[tunnel]\n{settings}\n");
            assert!(crate::validation_warnings::retired_section_tombstones(&raw).is_empty());
            for result in [parse_current(&raw), parse_current_resilient(&raw)] {
                let error = result.unwrap_err();
                assert!(error.is::<RetiredSecurityConfigError>(), "{error:#}");
                assert!(error.to_string().contains("host-managed services"));
                assert!(!format!("{error:#}").contains("private-sentinel"));
            }
        }
    }

    #[test]
    fn retired_security_absent_empty_and_serialized_defaults_are_accepted() {
        for suffix in [
            "",
            "[security.nevis]\n[security.webauthn]\n[node_transport]\n",
            r#"
[security.nevis]
enabled = false
instance_url = ""
realm = "master"
client_id = ""
client_secret = ""
token_validation = "local"
jwks_url = ""
role_mapping = []
require_mfa = false
session_timeout_secs = 3600
[security.webauthn]
enabled = false
rp_id = "localhost"
rp_origin = "http://localhost:42617"
rp_name = "ReiClaw"
[node_transport]
enabled = true
shared_secret = ""
max_request_age_secs = 300
require_https = true
allowed_peers = []
tls_cert_path = ""
tls_key_path = ""
mutual_tls = false
connection_pool_size = 4
"#,
        ] {
            let raw = format!("schema_version = 3\n{suffix}");
            parse_current(&raw).unwrap();
            let config = parse_current_resilient(&raw).unwrap();
            assert!(config.degraded_sections.is_empty());
            let serialized = toml::to_string(&config).unwrap();
            for surface in SURFACES {
                assert!(!serialized.contains(&format!("[{surface}]")));
            }
        }
    }

    #[test]
    fn retired_security_rejects_configured_policy_and_malformed_values_before_salvage() {
        for (surface, field) in [
            ("security.nevis", "enabled = true"),
            ("security.nevis", "client_secret = \"private-sentinel\""),
            ("security.nevis", "realm = \"private-sentinel\""),
            ("security.nevis", "require_mfa = true"),
            (
                "security.nevis",
                "role_mapping = [{ nevis_role = \"private-sentinel\" }]",
            ),
            ("security.nevis", "enabled = \"false\""),
            ("security.nevis", "future_policy = \"private-sentinel\""),
            ("security.webauthn", "enabled = true"),
            (
                "security.webauthn",
                "rp_origin = \"https://private-sentinel\"",
            ),
            ("security.webauthn", "rp_id = []"),
            (
                "node_transport",
                "shared_secret = \"vault:private-sentinel\"",
            ),
            ("node_transport", "allowed_peers = [\"private-sentinel\"]"),
            ("node_transport", "tls_cert_path = \"private-sentinel\""),
            ("node_transport", "tls_key_path = \"private-sentinel\""),
            ("node_transport", "mutual_tls = true"),
            ("node_transport", "require_https = false"),
            ("node_transport", "max_request_age_secs = 301"),
            ("node_transport", "allowed_peers = \"\""),
        ] {
            let raw = format!("schema_version = 3\n[{surface}]\n{field}\n");
            assert!(crate::validation_warnings::retired_section_tombstones(&raw).is_empty());
            for result in [parse_current(&raw), parse_current_resilient(&raw)] {
                let error = result.unwrap_err();
                assert!(error.is::<RetiredSecurityConfigError>(), "{error:#}");
                assert!(error.to_string().contains(surface));
                assert!(!format!("{error:#}").contains("private-sentinel"));
            }
        }
        for path in SURFACES {
            for value in ["false", "[]", "\"private-sentinel\""] {
                let raw = format!("schema_version = 3\n{path} = {value}\n");
                assert!(
                    parse_current_resilient(&raw)
                        .unwrap_err()
                        .is::<RetiredSecurityConfigError>()
                );
            }
        }
    }

    #[test]
    fn retired_security_env_never_hides_configured_or_unknown_policy() {
        for (tail, raw) in [
            ("security__nevis__client_secret", "private-sentinel"),
            ("security__nevis__enabled", "true"),
            ("security__nevis__enabled", "\"false\""),
            ("security__nevis__future_policy", ""),
            ("security__nevis__role_mapping__private_sentinel", "{}"),
            ("security__webauthn", "{ enabled = true }"),
            ("node_transport__allowed_peers", "[\"private-sentinel\"]"),
            ("node_transport__tls_key_path", "private-sentinel"),
            ("node_transport__require_https", "false"),
            ("node_transport__mutual_tls", "true"),
            (
                "node_transport__connection_pool_size",
                "4\nprivate = 'private-sentinel'",
            ),
            ("node_transport", "private-sentinel"),
        ] {
            let error = reject_retired_security_env(tail, raw).unwrap_err();
            assert!(!format!("{error:#}").contains("private-sentinel"));
        }
        for (tail, raw) in [
            ("security__nevis", "{}"),
            ("security__nevis__enabled", "false"),
            ("security__nevis__client_secret", ""),
            ("security__nevis__client_secret", "\"\""),
            ("security__nevis__realm", "master"),
            ("security__webauthn__rp_id", "localhost"),
            ("node_transport__allowed_peers", "[]"),
            ("node_transport__require_https", "true"),
            ("node_transport__max_request_age_secs", "300"),
            ("node_transport__connection_pool_size", "4"),
        ] {
            reject_retired_security_env(tail, raw).unwrap();
        }
    }

    #[tokio::test]
    async fn retired_security_save_and_comment_paths_preserve_operator_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        for raw in [
            "schema_version = 3\n[node_transport]\nshared_secret = 'private-sentinel'\n",
            "schema_version = 3\n[tunnel.ngrok]\nauth_token = 'private-sentinel'\n",
        ] {
            tokio::fs::write(&path, raw).await.unwrap();
            let mut config = crate::schema::Config {
                config_path: path.clone(),
                data_dir: temp.path().join("data"),
                loaded_from: Some(path.clone()),
                ..Default::default()
            };
            config.channels.show_tool_calls = true;
            config.mark_dirty("channels.show_tool_calls");
            for mode in ["save", "save_dirty", "force_save"] {
                let result = match mode {
                    "save" => config.save().await,
                    "save_dirty" => config.save_dirty().await,
                    _ => config.force_save().await,
                };
                let error = result.unwrap_err();
                assert!(error.is::<RetiredSecurityConfigError>(), "{error:#}");
                assert!(!format!("{error:#}").contains("private-sentinel"));
                assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), raw);
            }
            let error = crate::comment_writer::apply_comments(
                &path,
                &[("schema_version".into(), "updated".into())],
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(!error.to_string().contains("private-sentinel"));
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), raw);
        }
    }
}
