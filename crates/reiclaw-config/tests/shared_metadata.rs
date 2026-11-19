//! Configurable core discovery and display adapters share one schema contract.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use reiclaw_api::config_metadata::{
    ChannelInfo, ConfigTab, CredentialSurfaceClass, NestedOptionEntry, PropFieldSpec, PropKind,
    SecretFieldInfo,
};
use reiclaw_config::schema::{Config, TelegramConfig};
use reiclaw_config::traits::{AliasSource, ChannelConfig, PropFieldInfo};
use reiclaw_macros::Configurable;

// Configurable's generated paths resolve through the consuming crate's shims.
mod config {
    pub use reiclaw_config::config::*;
}
mod security {
    pub use reiclaw_config::secrets::SecretStore;
}
mod traits {
    pub use reiclaw_config::traits::*;
}

fn assert_same_schema(mut core: Vec<PropFieldSpec>, display: Vec<PropFieldInfo>) {
    let mut displayed: Vec<_> = display.iter().map(PropFieldInfo::spec).collect();
    core.sort_by(|a, b| a.name.cmp(&b.name));
    displayed.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(core, displayed);
}

#[test]
fn generated_property_and_secret_metadata_use_api_types() {
    let mut channel = TelegramConfig::default();
    channel
        .set_prop("channels.telegram.bot_token", "test-token")
        .unwrap();
    let fields = channel.prop_fields();
    let token = fields
        .iter()
        .find(|field| field.name == "channels.telegram.bot_token")
        .unwrap();
    let kind: PropKind = token.kind;
    let tab: ConfigTab = token.tab;
    assert_eq!(kind, PropKind::String);
    assert_eq!(tab, ConfigTab::Connection);
    assert!(token.is_secret);
    assert!(!token.display_value.contains("test-token"));
    let secrets: Vec<SecretFieldInfo> = channel.secret_fields();
    assert!(
        secrets
            .iter()
            .any(|field| field.name == token.name && field.is_set)
    );

    let provider = reiclaw_config::schema::OpenAITtsProviderConfig::default();
    let credential = provider
        .prop_fields()
        .into_iter()
        .find(|field| field.name.ends_with(".api_key"))
        .unwrap();
    let class: Option<CredentialSurfaceClass> = credential.credential_class;
    assert_eq!(class, Some(CredentialSurfaceClass::EncryptedSecret));

    // Both compatibility paths must resolve to the canonical API type.
    let _: reiclaw_config::traits::PropKind = kind;
    let _: reiclaw_config::config::ConfigTab = tab;
    let _: Option<reiclaw_config::traits::CredentialSurfaceClass> = class;
}

#[test]
fn channel_and_integration_metadata_remain_derived_from_config() {
    let mut config = Config::default();
    let channels: Vec<ChannelInfo> = config.channels.channels();
    let telegram = channels
        .iter()
        .find(|channel| channel.kind == "telegram")
        .unwrap();
    assert_eq!(telegram.name, <TelegramConfig as ChannelConfig>::name());
    assert_eq!(telegram.desc, <TelegramConfig as ChannelConfig>::desc());
    assert!(!telegram.configured);
    config
        .channels
        .telegram
        .insert("test".into(), TelegramConfig::default());
    let channels: Vec<reiclaw_config::traits::ChannelInfo> = config.channels.channels();
    assert!(
        channels
            .iter()
            .find(|channel| channel.kind == "telegram")
            .unwrap()
            .configured
    );
}

#[test]
fn nested_option_metadata_uses_current_config_presence() {
    let mut config = Config::default();
    let before: Vec<NestedOptionEntry> = config.tunnel.nested_option_entries();
    let before = before
        .iter()
        .find(|entry| entry.field == "tailscale")
        .unwrap();
    assert!(!before.present);
    config.tunnel.tailscale = Some(Default::default());
    let after: Vec<reiclaw_config::traits::NestedOptionEntry> =
        config.tunnel.nested_option_entries();
    let after = after
        .iter()
        .find(|entry| entry.field == "tailscale")
        .unwrap();
    assert!(after.present);
    assert_eq!(before.display_name, after.display_name);
    assert_eq!(before.description, after.description);
}

#[test]
fn core_schema_matches_display_for_current_config_and_populated_options() {
    let mut config = Config::default();
    assert_same_schema(config.prop_schema(), config.prop_fields());
    config.init_defaults(None);
    config
        .providers
        .models
        .openai
        .insert("ops.eu".into(), Default::default());
    config
        .http_request
        .secrets
        .insert("service.token".into(), "fixture-only".into());
    config
        .mcp
        .servers
        .push(reiclaw_config::schema::McpServerConfig {
            name: "fs.docs".into(),
            ..Default::default()
        });
    assert_same_schema(config.prop_schema(), config.prop_fields());
    assert!(
        config
            .prop_paths()
            .contains(&"providers.models.openai.ops.eu.api_key".into())
    );
    assert!(
        config
            .prop_paths()
            .contains(&"http_request.secrets.service.token".into())
    );

    let generated = reiclaw_config::generate::generate(&Default::default()).unwrap();
    let example = reiclaw_config::current::parse_current(&generated).unwrap();
    assert_same_schema(example.prop_schema(), example.prop_fields());
}

static SERIALIZATIONS: AtomicUsize = AtomicUsize::new(0);

fn counted_bool<S: serde::Serializer>(value: &bool, serializer: S) -> Result<S::Ok, S::Error> {
    SERIALIZATIONS.fetch_add(1, Ordering::SeqCst);
    value.serialize(serializer)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[prefix = "probe.entry"]
struct ObservedEntry {
    #[serde(default)]
    name: String,
    #[serde(default, serialize_with = "counted_bool")]
    enabled: bool,
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    token: String,
    #[serde(default)]
    #[derived_from_secret]
    derived: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[prefix = "probe"]
struct DiscoveryProbe {
    #[serde(default)]
    #[nested]
    leaf: ObservedEntry,
    #[serde(flatten)]
    #[nested]
    flattened: ObservedEntry,
    #[serde(default)]
    #[nested]
    optional: Option<ObservedEntry>,
    #[serde(default)]
    #[nested]
    aliases: HashMap<String, ObservedEntry>,
    #[serde(default)]
    #[nested]
    families: HashMap<String, HashMap<String, ObservedEntry>>,
    #[serde(default)]
    #[nested]
    #[natural_key = "name"]
    entries: Vec<ObservedEntry>,
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    secrets: HashMap<String, String>,
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    optional_secrets: Option<HashMap<String, String>>,
}

#[test]
fn core_discovery_does_not_serialize_values_in_any_nested_shape() {
    let entry = ObservedEntry {
        name: "north.east".into(),
        enabled: true,
        token: "fixture-secret-never-in-core".into(),
        derived: "fixture-derived-never-in-core".into(),
    };
    let mut probe = DiscoveryProbe {
        leaf: entry.clone(),
        flattened: entry.clone(),
        optional: Some(entry.clone()),
        aliases: HashMap::from([("a".into(), entry.clone()), ("a.b".into(), entry.clone())]),
        families: HashMap::from([(
            "family.with.dots".into(),
            HashMap::from([("alias.with.dots".into(), entry.clone())]),
        )]),
        // Duplicate and empty natural keys must remain visible for validation;
        // enumeration must not deduplicate or accidentally expose the key leaf.
        entries: vec![entry.clone(), entry, ObservedEntry::default()],
        secrets: HashMap::from([
            ("a".into(), "fixture-a".into()),
            ("a.b".into(), "fixture-b".into()),
        ]),
        optional_secrets: Some(HashMap::from([(
            "token.with.dots".into(),
            "fixture-c".into(),
        )])),
    };
    SERIALIZATIONS.store(0, Ordering::SeqCst);
    let specs = probe.prop_schema();
    let paths = probe.prop_paths();
    assert_eq!(SERIALIZATIONS.load(Ordering::SeqCst), 0);
    assert_eq!(
        paths,
        specs
            .iter()
            .map(|field| field.name.clone())
            .collect::<Vec<_>>()
    );
    assert!(paths.contains(&"probe.families.family.with.dots.alias.with.dots.enabled".into()));
    assert!(paths.contains(&"probe.entries.<unnamed-2>.enabled".into()));
    assert!(paths.contains(&"probe.optional_secrets.token.with.dots".into()));
    assert_eq!(
        paths
            .iter()
            .filter(|p| *p == "probe.entries.north.east.enabled")
            .count(),
        2
    );
    assert!(!paths.contains(&"probe.entries.north.east.name".into()));
    assert!(!format!("{specs:?}").contains("fixture-secret-never-in-core"));
    let token = specs
        .iter()
        .find(|field| field.name == "probe.leaf.token")
        .unwrap();
    assert_eq!(token.kind, PropKind::String);
    assert!(token.is_secret);
    assert_eq!(
        token.credential_class,
        Some(CredentialSurfaceClass::EncryptedSecret)
    );
    assert!(
        specs
            .iter()
            .any(|field| field.name == "probe.leaf.derived" && field.derived_from_secret)
    );
    assert_eq!(
        specs
            .iter()
            .find(|field| field.name == "probe.leaf.enabled")
            .unwrap()
            .kind,
        PropKind::Bool
    );

    let display = probe.prop_fields();
    assert!(
        SERIALIZATIONS.load(Ordering::SeqCst) > 0,
        "display must exercise the serialization probe"
    );
    assert_same_schema(specs, display);
    probe.optional = None;
    probe.optional_secrets = None;
    assert_same_schema(probe.prop_schema(), probe.prop_fields());
    assert!(
        !probe
            .prop_paths()
            .iter()
            .any(|path| path.starts_with("probe.entry."))
    );
}

#[test]
fn typed_alias_sources_preserve_complete_keys_and_sort_outputs() {
    let mut config = Config::default();
    config
        .providers
        .models
        .openai
        .insert("ops.eu".into(), Default::default());
    config
        .providers
        .models
        .openai
        .insert("ops".into(), Default::default());
    config
        .providers
        .tts
        .openai
        .insert("voice.eu".into(), Default::default());
    config
        .providers
        .transcription
        .openai
        .insert("speech_eu".into(), Default::default());
    config
        .channels
        .telegram
        .insert("bot.eu".into(), Default::default());
    config
        .risk_profiles
        .insert("risk.eu".into(), Default::default());
    for (source, expected) in [
        (
            AliasSource::ModelProviders,
            vec!["openai.ops", "openai.ops.eu"],
        ),
        (AliasSource::TtsProviders, vec!["openai.voice.eu"]),
        (
            AliasSource::TranscriptionProviders,
            vec!["openai.speech_eu"],
        ),
        (AliasSource::Channels, vec!["telegram.bot.eu"]),
        (AliasSource::RiskProfiles, vec!["risk.eu"]),
        (AliasSource::Agents, vec![]),
    ] {
        assert_eq!(config.resolve_alias_source(source), expected);
    }
}

#[test]
fn typed_channel_walk_reads_enabled_and_covers_every_declared_alias_map() {
    let mut config = Config::default();
    for section in Config::map_key_sections().into_iter().filter(|section| {
        section.path.starts_with("channels.")
            && section.kind == reiclaw_config::traits::MapKeyKind::Map
    }) {
        config.create_map_key(section.path, "off").unwrap();
        config
            .set_prop(&format!("{}.off.enabled", section.path), "false")
            .unwrap();
    }
    let declared: BTreeSet<_> = Config::map_key_sections()
        .into_iter()
        .filter_map(|section| section.path.strip_prefix("channels."))
        .collect();
    let walked: BTreeSet<_> = config
        .channels
        .iter_aliases()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(
        walked, declared,
        "new channel maps must participate in the typed walk"
    );
    assert!(!config.channels.has_any_enabled());

    config.channels.telegram.insert(
        "ops.eu".into(),
        TelegramConfig {
            enabled: true,
            ..Default::default()
        },
    );
    config.agents.insert(
        "owner".into(),
        reiclaw_config::schema::AliasedAgentConfig {
            enabled: true,
            channels: vec!["telegram.ops.eu".into()],
            ..Default::default()
        },
    );
    let aliases = config.channels_by_alias();
    let enabled: Vec<_> = aliases.iter().filter(|entry| entry.enabled).collect();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].alias, "ops.eu");
    assert_eq!(enabled[0].owning_agent.as_deref(), Some("owner"));
    assert!(config.channels.has_any_enabled());
    assert!(aliases.windows(2).all(|pair| {
        (&pair[0].channel_type, &pair[0].alias) < (&pair[1].channel_type, &pair[1].alias)
    }));
    assert!(
        aliases
            .iter()
            .all(|entry| !reiclaw_config::schema::is_retired_transport(&entry.channel_type))
    );
}
