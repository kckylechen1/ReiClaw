//! Explicit device and bounded owner permissions for Gateway bridge credentials.
use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// Restricted edge access; the existing bridge owns credentials and session scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct GatewayDeviceBridgeConfig {
    pub agent: String,
    /// Public-tier chat policy. Required for chat, optional for delegation-only bridges.
    pub risk_profile: Option<String>,
    pub operations: Vec<String>,
}

/// ADR-023 A2 fixed deny set, derived from the tools/runtime registries.
/// Keep this independent of optional tool features and of client-selected surfaces.
pub const DEVICE_DENIED_TOOLS: &[&str] = &[
    "shell",
    "process",
    "file_read",
    "file_write",
    "file_edit",
    "file_download",
    "file_upload",
    "file_upload_bundle",
    "glob_search",
    "content_search",
    "git_operations",
    "backup",
    "data_management",
    "notify",
    "send_message_to_peer",
    "ask_user",
    "escalate_to_human",
    "reaction",
    "poll",
    "channel_room",
    "memory_store",
    "memory_recall",
    "memory_forget",
    "memory_export",
    "memory_purge",
    "note_owner_correction",
    "sessions_list",
    "sessions_history",
    "sessions_send",
    "sessions_current",
    "cron_add",
    "cron_list",
    "cron_remove",
    "cron_update",
    "cron_run",
    "cron_runs",
    "schedule",
    "tool_search",
];

/// Shared by configuration validation and credential admission, including reloads.
pub fn validate_device_profile(
    profile: &crate::schema::RiskProfileConfig,
) -> Result<(), &'static str> {
    use crate::autonomy::{AutonomyLevel, McpDiscoveredToolPolicy};
    if profile.level == AutonomyLevel::Full {
        return Err("device risk profile must not use full autonomy");
    }
    let allowed = profile
        .allowed_tools
        .as_ref()
        .ok_or("device risk profile requires allowed_tools")?;
    if profile.mcp_discovered_tool_policy != McpDiscoveredToolPolicy::ExplicitOnly {
        return Err("device MCP policy must be explicit_only");
    }
    if !profile.always_ask.is_empty() || profile.approval_route.is_some() {
        return Err("device risk profile must not prompt or route approvals");
    }
    for tool in allowed {
        if DEVICE_DENIED_TOOLS.contains(&tool.as_str()) {
            return Err("device risk profile contains a denied tool");
        }
        let full_mcp_name = tool.split_once("__").is_some_and(|(server, name)| {
            !server.is_empty()
                && !name.is_empty()
                && tool
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        });
        if tool != "web_search_tool" && !full_mcp_name {
            return Err("public device tools must be web_search_tool or full server__tool names");
        }
        if !profile.auto_approve.contains(tool) {
            return Err("every device allowed tool must be explicitly auto_approved");
        }
    }
    Ok(())
}

/// Whether two bridge namespaces share any exact session or prefix.
pub fn namespaces_overlap(
    a: &crate::schema::GatewayBridgeConfig,
    b: &crate::schema::GatewayBridgeConfig,
) -> bool {
    a.sessions.iter().any(|s| b.allows_session(s))
        || b.sessions.iter().any(|s| a.allows_session(s))
        || a.session_prefix
            .as_deref()
            .filter(|p| !p.is_empty())
            .is_some_and(|a| {
                b.session_prefix
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .is_some_and(|b| a.starts_with(b) || b.starts_with(a))
            })
}

impl GatewayDeviceBridgeConfig {
    pub fn validated_profile<'a>(
        &self,
        config: &'a crate::schema::Config,
    ) -> Result<&'a crate::schema::RiskProfileConfig, &'static str> {
        let name = self
            .risk_profile
            .as_deref()
            .ok_or("device chat requires risk_profile")?;
        let profile = config
            .risk_profiles
            .get(name)
            .ok_or("device risk profile does not exist")?;
        validate_device_profile(profile)?;
        Ok(profile)
    }

    pub fn valid_operation(operation: &str) -> bool {
        matches!(
            operation,
            "chat"
                | "answer"
                | "cancel"
                | "tachi_start"
                | "tachi_status"
                | "tachi_result"
                | "tachi_cancel"
        )
    }

    pub fn allows(&self, agent: &str, operation: &str) -> bool {
        !self.agent.is_empty()
            && self.agent == agent
            && self.operations.iter().all(|op| Self::valid_operation(op))
            && self.operations.iter().any(|op| op == operation)
    }
}

/// Explicit owner trust, bounded to one agent and once-only approvals.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct GatewayOwnerBridgeConfig {
    pub agent: String,
    pub operations: Vec<String>,
}

impl GatewayOwnerBridgeConfig {
    pub fn allows(&self, agent: &str, operation: &str) -> bool {
        !self.agent.is_empty()
            && self.agent == agent
            && self
                .operations
                .iter()
                .all(|op| matches!(op.as_str(), "chat" | "cancel" | "approval_response"))
            && self.operations.iter().any(|op| op == operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_grants_are_explicit_and_do_not_add_chat_cancel_or_owner_authority() {
        let mut access = GatewayDeviceBridgeConfig {
            agent: "edge".into(),
            risk_profile: None,
            operations: vec!["chat".into(), "cancel".into()],
        };
        for op in [
            "tachi_start",
            "tachi_status",
            "tachi_result",
            "tachi_cancel",
        ] {
            assert!(!access.allows("edge", op));
        }
        access.operations = vec!["tachi_status".into()];
        assert!(access.allows("edge", "tachi_status"));
        assert!(!access.allows("edge", "tachi_start"));
        assert!(!access.allows("edge", "cancel"));
        assert!(!access.allows("edge", "approval_response"));
        assert!(!access.allows("other", "tachi_status"));
        access.operations.push("tachi_shell".into());
        assert!(!access.allows("edge", "tachi_status"));
    }

    #[test]
    fn missing_permissions_and_unknown_permissions_fail_closed() {
        let mut access: GatewayDeviceBridgeConfig = toml::from_str("agent = 'device'").unwrap();
        assert!(!access.allows("device", "chat"));
        access.operations = vec!["chat".into()];
        assert!(access.allows("device", "chat"));
        assert!(!access.allows("other", "chat"));
        assert!(!access.allows("device", "approval_response"));
        access.operations.push("approve".into());
        assert!(!access.allows("device", "chat"));
        assert!(
            toml::from_str::<GatewayDeviceBridgeConfig>("agent = 'device'\nallow_owner = true")
                .is_err()
        );
    }

    #[test]
    fn restriction_survives_configuration_roundtrip() {
        let bridge: crate::schema::GatewayBridgeConfig = toml::from_str(
            "sessions = ['device-session']\n[device]\nagent = 'device'\noperations = ['chat', 'cancel']",
        ).unwrap();
        let encoded = toml::to_string(&bridge).unwrap();
        let decoded: crate::schema::GatewayBridgeConfig = toml::from_str(&encoded).unwrap();
        let access = decoded.device.unwrap();
        assert!(access.allows("device", "chat"));
        assert!(access.allows("device", "cancel"));
        assert!(!access.allows("device", "answer"));
        assert!(
            crate::schema::GatewayBridgeConfig::default()
                .device
                .is_none()
        );
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    #[test]
    fn owner_approval_is_opt_in_and_unknown_grants_fail_closed() {
        let absent: crate::schema::GatewayBridgeConfig = toml::from_str("").unwrap();
        assert!(absent.owner.is_none());
        let mut owner: GatewayOwnerBridgeConfig = toml::from_str("agent = 'owner'").unwrap();
        assert!(!owner.allows("owner", "approval_response"));
        owner.operations = vec!["chat".into(), "approval_response".into()];
        assert!(owner.allows("owner", "approval_response"));
        assert!(!owner.allows("other", "approval_response"));
        assert!(!owner.allows("owner", "answer"));
        owner.operations.push("always".into());
        assert!(!owner.allows("owner", "chat"));
        let device = GatewayDeviceBridgeConfig {
            agent: "owner".into(),
            risk_profile: None,
            operations: vec!["chat".into(), "approval_response".into()],
        };
        assert!(!device.allows("owner", "approval_response"));
    }
}

#[cfg(test)]
mod confinement_tests {
    use super::*;
    use crate::schema::{Config, GatewayBridgeConfig, RiskProfileConfig};

    fn public_profile() -> RiskProfileConfig {
        RiskProfileConfig {
            allowed_tools: Some(vec!["web_search_tool".into(), "search__lookup".into()]),
            auto_approve: vec!["web_search_tool".into(), "search__lookup".into()],
            always_ask: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn device_profile_rejects_each_authority_escape() {
        assert!(validate_device_profile(&public_profile()).is_ok());
        for tool in DEVICE_DENIED_TOOLS.iter().copied().chain([
            "*",
            "lookup",
            "search__*",
            "__lookup",
            "search__",
            "pipeline",
        ]) {
            let mut profile = public_profile();
            profile.allowed_tools = Some(vec![tool.into()]);
            profile.auto_approve = vec![tool.into()];
            assert!(validate_device_profile(&profile).is_err(), "{tool}");
        }
        for mutation in 0..6 {
            let mut profile = public_profile();
            match mutation {
                0 => profile.level = crate::autonomy::AutonomyLevel::Full,
                1 => profile.allowed_tools = None,
                2 => profile.auto_approve.clear(),
                3 => profile.always_ask.push("web_search_tool".into()),
                4 => {
                    profile.mcp_discovered_tool_policy =
                        crate::autonomy::McpDiscoveredToolPolicy::AutoAdmit
                }
                5 => {
                    profile.approval_route =
                        Some(toml::from_str("approver_channel = 'owner'").unwrap())
                }
                _ => unreachable!(),
            }
            assert!(
                validate_device_profile(&profile).is_err(),
                "mutation {mutation}"
            );
        }
        let mut config = Config::default();
        let device = GatewayDeviceBridgeConfig {
            risk_profile: Some("public".into()),
            ..Default::default()
        };
        assert!(device.validated_profile(&config).is_err());
        config
            .risk_profiles
            .insert("public".into(), public_profile());
        assert!(device.validated_profile(&config).is_ok());
    }

    #[test]
    fn device_namespaces_reject_exact_prefix_and_nested_prefix_collisions() {
        let device = GatewayBridgeConfig {
            sessions: vec!["device-one".into()],
            session_prefix: Some("edge-".into()),
            ..Default::default()
        };
        for other in [
            GatewayBridgeConfig {
                sessions: vec!["device-one".into()],
                ..Default::default()
            },
            GatewayBridgeConfig {
                sessions: vec!["edge-two".into()],
                ..Default::default()
            },
            GatewayBridgeConfig {
                session_prefix: Some("device-".into()),
                ..Default::default()
            },
            GatewayBridgeConfig {
                session_prefix: Some("edge-sub-".into()),
                ..Default::default()
            },
            GatewayBridgeConfig {
                session_prefix: Some("edge".into()),
                ..Default::default()
            },
        ] {
            assert!(namespaces_overlap(&device, &other));
        }
        assert!(!namespaces_overlap(
            &device,
            &GatewayBridgeConfig {
                sessions: vec!["owner".into()],
                session_prefix: Some("other-".into()),
                ..Default::default()
            }
        ));
    }

    #[test]
    fn config_validation_enforces_device_profiles_and_namespace_isolation() {
        let mut config = Config::default();
        config.gateway.require_pairing = true;
        config
            .risk_profiles
            .insert("public".into(), public_profile());
        config
            .providers
            .models
            .ensure("ollama", "local")
            .unwrap()
            .model = Some("fixture".into());
        config.agents.insert(
            "edge".into(),
            crate::schema::AliasedAgentConfig {
                model_provider: "ollama.local".into(),
                risk_profile: "public".into(),
                ..Default::default()
            },
        );
        config.gateway.bridges.insert(
            "device".into(),
            GatewayBridgeConfig {
                token_hash: "0f".repeat(32),
                sessions: vec!["device-one".into()],
                session_prefix: Some("edge-".into()),
                device: Some(GatewayDeviceBridgeConfig {
                    agent: "edge".into(),
                    risk_profile: Some("public".into()),
                    operations: vec!["chat".into()],
                }),
                ..Default::default()
            },
        );
        config.validate().expect("valid public bridge");

        for scenario in [
            "missing",
            "unknown",
            "full",
            "unbounded",
            "denied",
            "mcp",
            "approval",
            "prompt",
            "route",
        ] {
            let mut invalid = config.clone();
            let device = invalid
                .gateway
                .bridges
                .get_mut("device")
                .unwrap()
                .device
                .as_mut()
                .unwrap();
            let profile = invalid.risk_profiles.get_mut("public").unwrap();
            match scenario {
                "missing" => device.risk_profile = None,
                "unknown" => device.risk_profile = Some("absent".into()),
                "full" => profile.level = crate::autonomy::AutonomyLevel::Full,
                "unbounded" => profile.allowed_tools = None,
                "denied" => {
                    profile.allowed_tools = Some(vec!["shell".into()]);
                    profile.auto_approve = vec!["shell".into()];
                }
                "mcp" => {
                    profile.mcp_discovered_tool_policy =
                        crate::autonomy::McpDiscoveredToolPolicy::AutoAdmit
                }
                "approval" => profile.auto_approve.clear(),
                "prompt" => profile.always_ask.push("web_search_tool".into()),
                "route" => {
                    profile.approval_route =
                        Some(toml::from_str("approver_channel = 'owner'").unwrap())
                }
                _ => unreachable!(),
            }
            let error = invalid.validate().expect_err(scenario).to_string();
            assert!(
                error.contains("gateway.bridges.device.device.risk_profile"),
                "{scenario}: {error}"
            );
        }

        let mut delegation = config.clone();
        let device = delegation
            .gateway
            .bridges
            .get_mut("device")
            .unwrap()
            .device
            .as_mut()
            .unwrap();
        device.risk_profile = None;
        device.operations = vec!["tachi_start".into(), "tachi_status".into()];
        delegation
            .validate()
            .expect("delegation-only bridge needs no chat profile");
        delegation
            .gateway
            .bridges
            .get_mut("device")
            .unwrap()
            .device
            .as_mut()
            .unwrap()
            .risk_profile = Some("absent".into());
        assert!(
            delegation
                .validate()
                .unwrap_err()
                .to_string()
                .contains("risk_profile")
        );

        for (sessions, prefix, overlap) in [
            (vec!["device-one"], None, true),
            (vec!["edge-two"], None, true),
            (vec![], Some("device-"), true),
            (vec![], Some("edge-sub-"), true),
            (vec![], Some("edge"), true),
            (vec!["owner"], Some("other-"), false),
        ] {
            let mut candidate = config.clone();
            candidate.gateway.bridges.insert(
                "other".into(),
                GatewayBridgeConfig {
                    token_hash: "a1".repeat(32),
                    sessions: sessions.into_iter().map(str::to_owned).collect(),
                    session_prefix: prefix.map(str::to_owned),
                    ..Default::default()
                },
            );
            if overlap {
                let error = candidate.validate().unwrap_err().to_string();
                assert!(error.contains("session namespace overlaps"), "{error}");
            } else {
                candidate.validate().expect("disjoint owner namespace");
            }
        }
    }
}
