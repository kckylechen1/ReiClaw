//! List supported channel configuration surfaces and their actual transport owners.

use reiclaw_api::config_metadata::ChannelInfo;
use reiclaw_config::schema::ChannelsConfig;

struct ChannelSupportSpec {
    /// Display name from the channel configuration inventory. ACP is a separate
    /// bridge and has no row in that inventory.
    schema_name: Option<&'static str>,
    /// Accepted config/API type keys. Include legacy underscore aliases where
    /// earlier channel references allowed them.
    type_keys: &'static [&'static str],
    supported: bool,
}

// Single source of truth for both display inventory and per-config type
// support. Keep this schema-backed: tests assert enabled display names exist
// in the config crate's canonical channel inventory and that keys do not drift.
const CHANNEL_SUPPORT_SPECS: &[ChannelSupportSpec] = &[
    ChannelSupportSpec {
        schema_name: Some("Telegram"),
        type_keys: &["telegram"],
        supported: true,
    },
    ChannelSupportSpec {
        schema_name: Some("Webhook"),
        type_keys: &["webhook"],
        supported: cfg!(feature = "gateway"),
    },
    ChannelSupportSpec {
        schema_name: None,
        type_keys: &["acp-server", "acp_server"],
        supported: true,
    },
];

fn supported_channel_names() -> impl Iterator<Item = &'static str> {
    CHANNEL_SUPPORT_SPECS
        .iter()
        .filter(|spec| spec.supported)
        .filter_map(|spec| spec.schema_name)
}

/// Configuration surfaces supported by this build. Telegram and ACP run in
/// separate bridge binaries; support here does not prove installation or health.
pub fn supported_channels(cfg: &ChannelsConfig) -> Vec<ChannelInfo> {
    cfg.channels()
        .into_iter()
        .filter(|info| !info.retired && supported_channel_names().any(|name| name == info.name))
        .collect()
}

/// Configured surfaces whose actual owner is absent from this build. The
/// retained in-process webhook requires the gateway feature; external bridges
/// are configured separately and are not controlled by empty Cargo flags.
pub fn configured_unsupported_channels(cfg: &ChannelsConfig) -> Vec<ChannelInfo> {
    cfg.channels()
        .into_iter()
        .filter(|info| info.configured && !info.retired && !is_channel_type_supported(info.kind))
        .collect()
}

/// Retired transports with a populated `[channels.<kind>]` section. The
/// section still parses (schema retirement is evidence-gated in #584), but
/// nothing consumes it — surfaces report these distinctly instead of
/// presenting them as configured channels.
pub fn configured_retired_channels(cfg: &ChannelsConfig) -> Vec<ChannelInfo> {
    cfg.channels()
        .into_iter()
        .filter(|info| info.configured && info.retired)
        .collect()
}

/// Returns whether this build supports a channel configuration type.
/// Accepts both kebab-case keys emitted by the config schema and legacy
/// underscore spellings used in channel references.
pub fn is_channel_type_supported(channel_type: &str) -> bool {
    for spec in CHANNEL_SUPPORT_SPECS {
        if spec.type_keys.contains(&channel_type) {
            return spec.supported;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{CHANNEL_SUPPORT_SPECS, ChannelsConfig};
    use super::{
        configured_retired_channels, configured_unsupported_channels, is_channel_type_supported,
        supported_channels,
    };
    use std::collections::BTreeSet;

    #[test]
    fn channel_support_tracks_transport_owners() {
        assert!(is_channel_type_supported("telegram"));
        assert_eq!(
            is_channel_type_supported("webhook"),
            cfg!(feature = "gateway")
        );
        assert!(is_channel_type_supported("acp-server"));
        // Retired channel types (inbound webhook routes removed) resolve
        // as unknown, never as compiled.
        for retired in [
            "linq",
            "wati",
            "nextcloud_talk",
            "gmail_push",
            "whatsapp",
            "whatsapp-web",
            "whatsapp_web",
        ] {
            assert!(!is_channel_type_supported(retired), "{retired}");
        }
    }

    #[test]
    fn supported_channel_names_are_schema_names() {
        let cfg = ChannelsConfig::default();
        let schema_names: BTreeSet<_> = cfg.channels().into_iter().map(|info| info.name).collect();

        for name in CHANNEL_SUPPORT_SPECS
            .iter()
            .filter_map(|spec| spec.schema_name)
        {
            assert!(
                schema_names.contains(name),
                "supported channel name `{name}` is missing from ChannelsConfig::channels()"
            );
        }
    }

    #[test]
    fn supported_channels_match_expected_schema_names() {
        let cfg = ChannelsConfig::default();
        let actual: BTreeSet<_> = supported_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();
        let expected: BTreeSet<_> = CHANNEL_SUPPORT_SPECS
            .iter()
            .filter(|spec| spec.supported)
            .filter(|spec| !reiclaw_config::schema::is_retired_transport(spec.type_keys[0]))
            .filter_map(|spec| spec.schema_name)
            .collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn configured_unsupported_channels_uses_schema_config_status() {
        // Webhook is owned by the gateway feature; bridge binaries do not
        // depend on any root channel feature.
        let mut cfg = ChannelsConfig::default();
        cfg.webhook.insert(
            "default".to_string(),
            reiclaw_config::schema::WebhookConfig::default(),
        );

        let names: BTreeSet<_> = configured_unsupported_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();

        assert_eq!(names.contains("Webhook"), !cfg!(feature = "gateway"));
    }

    #[test]
    fn schema_deleted_transports_have_no_listing_entries() {
        let cfg = ChannelsConfig::default();

        let supported: BTreeSet<_> = supported_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();
        let unsupported: BTreeSet<_> = configured_unsupported_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();
        let retired: BTreeSet<_> = configured_retired_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();

        for removed in [
            "Slack",
            "Mattermost",
            "iMessage",
            "Matrix",
            "Signal",
            "IRC",
            "Twitch",
            "Lark",
            "Feishu",
            "WhatsApp",
            "LINE",
            "DingTalk",
            "WeCom",
            "WeCom WS",
            "WeChat",
            "QQ",
            "Twitter",
            "Mochat",
            "Nostr",
            "ClawdTalk",
            "Reddit",
            "Bluesky",
            "Git",
            "Voice Call",
            "Voice Wake",
            "Voice Duplex",
            "AMQP",
        ] {
            assert!(!supported.contains(removed));
            assert!(!unsupported.contains(removed));
            assert!(!retired.contains(removed));
        }
    }

    #[test]
    fn channel_type_support_matches_inventory_specs() {
        for spec in CHANNEL_SUPPORT_SPECS {
            for key in spec.type_keys {
                assert_eq!(
                    is_channel_type_supported(key),
                    spec.supported,
                    "channel type key `{key}` drifted from its support spec"
                );
            }
        }

        assert!(!is_channel_type_supported("not-a-channel"));
    }

    #[test]
    fn channel_support_specs_do_not_duplicate_entries() {
        let mut seen_names = BTreeSet::new();
        let mut seen_keys = BTreeSet::new();

        for spec in CHANNEL_SUPPORT_SPECS {
            if let Some(name) = spec.schema_name {
                assert!(
                    seen_names.insert(name),
                    "supported channel name `{name}` appears more than once"
                );
            }

            for key in spec.type_keys {
                assert!(
                    seen_keys.insert(*key),
                    "supported channel type key `{key}` appears more than once"
                );
            }
        }
    }

    #[test]
    fn channel_support_specs_cover_schema_channel_types() {
        let live_types: Vec<_> = reiclaw_config::schema::Config::map_key_sections()
            .into_iter()
            .filter_map(|section| {
                section
                    .path
                    .strip_prefix("channels.")
                    .filter(|kind| !kind.contains('.'))
                    .map(str::to_string)
            })
            .collect();
        assert!(!live_types.is_empty(), "retained channel sample is empty");
        for channel_type in live_types {
            let kebab = channel_type.replace('_', "-");
            assert!(
                CHANNEL_SUPPORT_SPECS.iter().any(|spec| spec
                    .type_keys
                    .iter()
                    .any(|key| *key == channel_type || *key == kebab)),
                "schema channel type `{channel_type}` is missing from CHANNEL_SUPPORT_SPECS"
            );
        }
    }
}
