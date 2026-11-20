//! Current-schema loading, generation, retirement and secret-handling guarantees.

use reiclaw_config::current::{
    CURRENT_SCHEMA_VERSION, detect_version, ensure_disk_at_current_version, parse_current,
    parse_current_salvaged,
};
use reiclaw_config::generate::{GenerateOptions, encrypt_secret_strings, generate};
use reiclaw_config::schema::Config;
use reiclaw_config::secrets::SecretStore;
use reiclaw_config::validation_warnings::ValidationWarning;

const CURRENT_FIXTURE: &str = include_str!("../../fixtures/v3.toml");

#[test]
fn v3_retired_inbound_webhook_channel_loads_with_tombstone_warning() {
    // A V3 config still carrying a retired channel section, an agent binding
    // to it and a peer group on it must keep loading: the section is ignored
    // and reported as a structured tombstone warning, not a hard error.
    let mut value: toml::Value =
        toml::from_str(&generate(&GenerateOptions::default()).expect("generate current succeeds"))
            .expect("generated V3 parses");
    let root = value.as_table_mut().unwrap();
    let linq: toml::Value = toml::from_str(
        r#"
[default]
enabled = true
api_token = "linq-tok"
from_phone = "+15555550100"
"#,
    )
    .unwrap();
    root.get_mut("channels")
        .and_then(toml::Value::as_table_mut)
        .expect("generated V3 has channels")
        .insert("linq".into(), linq);
    let (agent_alias, agent) = root
        .get_mut("agents")
        .and_then(toml::Value::as_table_mut)
        .and_then(|agents| agents.iter_mut().next())
        .expect("generated V3 has an agent");
    let agent_alias = agent_alias.clone();
    agent
        .as_table_mut()
        .unwrap()
        .entry("channels")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .unwrap()
        .push("linq.default".into());
    let group: toml::Value = toml::from_str(&format!(
        "channel = \"linq\"\nagents = [\"{agent_alias}\"]\nexternal_peers = [\"+15551234567\"]\n"
    ))
    .unwrap();
    root.entry("peer_groups")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap()
        .insert("linq_default".into(), group);
    let raw = toml::to_string(&value).unwrap();

    let cfg = parse_current(&raw).expect("retired channel section must not fail load");
    cfg.validate()
        .expect("leftover bindings to a retired channel type must not fail validation");

    let warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "inbound_webhook_channel_removed");
    assert_eq!(warnings[0].path, "channels.linq");
}

#[test]
fn v3_retired_wss_section_loads_with_tombstone_warning() {
    // The daemon's JSON-RPC socket and WSS listener were retired. A V3
    // config still carrying `[wss]` must keep loading: the section is
    // ignored and reported as a structured tombstone warning.
    let mut value: toml::Value =
        toml::from_str(&generate(&GenerateOptions::default()).expect("generate current succeeds"))
            .expect("generated V3 parses");
    let wss: toml::Value = toml::from_str(
        r#"
enabled = true
bind = "0.0.0.0"
port = 9781
cert_path = "/etc/zeroclaw/cert.pem"
key_path = "/etc/zeroclaw/key.pem"
"#,
    )
    .unwrap();
    value.as_table_mut().unwrap().insert("wss".into(), wss);
    let raw = toml::to_string(&value).unwrap();

    let cfg = parse_current(&raw).expect("retired [wss] section must not fail load");
    cfg.validate()
        .expect("retired [wss] section must not fail validation");

    let warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "wss_transport_removed");
    assert_eq!(warnings[0].path, "wss");
}

#[test]
fn v3_retired_dead_feature_sections_load_with_tombstone_warnings() {
    // The A2A server/agent cards, the ZeroCode todotracker and the
    // never-wired conversational_ai section lost their last consumers and
    // were deleted. Configs still carrying them must keep loading, with one
    // structured tombstone warning each.
    for (path, body, code) in [
        ("a2a", "enabled = true", "a2a_server_removed"),
        ("todotracker", "enabled = true", "todo_tracker_removed"),
        (
            "conversational_ai",
            "enabled = true",
            "conversational_ai_removed",
        ),
    ] {
        let (raw, warnings) = load_with_retired_section(path, body);
        let cfg =
            parse_current(&raw).unwrap_or_else(|e| panic!("[{path}] must not fail load: {e:#}"));
        cfg.validate()
            .unwrap_or_else(|e| panic!("[{path}] must not fail validation: {e:#}"));
        assert_eq!(warnings.len(), 1, "[{path}]: {warnings:?}");
        assert_eq!(warnings[0].code, code, "[{path}]");
        assert_eq!(warnings[0].path, path);
    }
}

#[test]
fn v3_retired_plugins_section_loads_with_tombstone_warning() {
    // The WASM plugin host was retired (extensions go through MCP). A V3
    // config still carrying `[plugins]` must keep loading with a warning.
    let (_, warnings) = load_with_retired_section(
        "plugins",
        r#"
enabled = true
plugins_dir = "~/.zeroclaw/plugins"
auto_discover = true
max_plugins = 5

[security]
signature_mode = "strict"
trusted_publisher_keys = ["00"]

[limits]
call_fuel = 1000

[[entries]]
name = "weather"
config = { api_key = "k" }
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "wasm_plugins_removed");
    assert_eq!(warnings[0].path, "plugins");
}

#[test]
fn v3_retired_hardware_sections_load_with_tombstone_warnings() {
    // In-process hardware left the body (ADR-017: devices join as Nodes).
    // `[hardware]` and `[peripherals]` must keep loading with a warning.
    for (path, body) in [
        (
            "hardware",
            "enabled = true\ntransport = \"serial\"\nserial_port = \"/dev/ttyACM0\"\nbaud_rate = 115200\n",
        ),
        (
            "peripherals",
            "enabled = true\ndatasheet_dir = \"docs/datasheets\"\n\n[[boards]]\nboard = \"nucleo-f401re\"\ntransport = \"serial\"\npath = \"/dev/ttyACM0\"\n",
        ),
    ] {
        let (_, warnings) = load_with_retired_section(path, body);
        assert_eq!(warnings.len(), 1, "{path}: {warnings:?}");
        assert_eq!(warnings[0].code, "hardware_moved_out_of_core");
        assert_eq!(warnings[0].path, path);
    }
}

#[test]
fn v3_retired_saas_sections_load_with_tombstone_warnings() {
    // The vendor SaaS tool families were retired. Their sections must keep
    // loading with a warning, including nested tables and secrets.
    for (path, body) in [
        (
            "jira",
            "enabled = true\nbase_url = \"https://example.atlassian.net\"\napi_token = \"tok\"\nallowed_actions = [\"get_ticket\"]\n",
        ),
        (
            "linkedin",
            "enabled = true\napi_version = \"202602\"\n\n[content]\ntopics = [\"rust\"]\n\n[image]\nenabled = true\n",
        ),
        (
            "microsoft365",
            "enabled = true\ntenant_id = \"t\"\nclient_id = \"c\"\nauth_flow = \"device_code\"\n",
        ),
        (
            "google_workspace",
            "enabled = true\nallowed_services = [\"gmail\"]\n\n[[allowed_operations]]\nservice = \"gmail\"\nresource = \"users\"\nmethods = [\"list\"]\n",
        ),
        ("cloud_ops", "enabled = true\ndefault_cloud = \"aws\"\n"),
        (
            "project_intel",
            "enabled = true\ndefault_language = \"en\"\n",
        ),
    ] {
        let (_, warnings) = load_with_retired_section(path, body);
        assert_eq!(warnings.len(), 1, "{path}: {warnings:?}");
        assert_eq!(warnings[0].code, "saas_integration_removed");
        assert_eq!(warnings[0].path, path);
    }
}

#[test]
fn v3_retired_otel_backend_and_fields_load_with_tombstone_warnings() {
    // The OpenTelemetry exporter was removed. `backend = "otel"` and the
    // `otel_*` keys must keep loading: the backend falls back to `none` and
    // every retired key is reported, never silently dropped.
    let mut value: toml::Value =
        toml::from_str(&generate(&GenerateOptions::default()).expect("generate current succeeds"))
            .expect("generated V3 parses");
    let observability = value
        .as_table_mut()
        .unwrap()
        .entry("observability")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap();
    let retired: toml::Table = toml::from_str(
        r#"
backend = "otel"
otel_endpoint = "http://localhost:4318"
otel_service_name = "zeroclaw"
otel_genai_content = "redacted"
otel_genai_content_max_chars = 500
otel_tool_io = "full"
otel_tool_io_max_chars = 500

[otel_headers]
Authorization = "Bearer x"
"#,
    )
    .unwrap();
    observability.extend(retired);
    let raw = toml::to_string(&value).unwrap();

    let cfg = parse_current(&raw).expect("retired otel config must not fail load");
    cfg.validate()
        .expect("retired otel config must not fail validation");
    assert_eq!(
        cfg.observability.backend,
        reiclaw_config::schema::ObservabilityBackend::None
    );

    let warnings = reiclaw_config::validation_warnings::retired_field_tombstones(&raw);
    assert_eq!(warnings.len(), 8, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .all(|w| w.code == "otel_observability_removed"),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.path == "observability.backend"),
        "{warnings:?}"
    );
}

#[test]
fn v3_retired_backup_retention_settings_warn_without_scheduling_work() {
    for (section, body) in [
        (
            "backup",
            "enabled = true\nschedule_cron = '0 3 * * *'\nencrypt = true\n",
        ),
        (
            "data_retention",
            "enabled = true\nretention_days = 7\ndry_run = false\n",
        ),
    ] {
        let raw = format!("schema_version = 3\n[{section}]\n{body}");
        let cfg = parse_current(&raw).expect("retired operator engine has a tombstone");
        let warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
        assert_eq!(warnings.len(), 1, "{section}");
        assert_eq!(warnings[0].code, "backup_retention_engine_removed");
        assert_eq!(warnings[0].path, section);
        assert!(warnings[0].message.contains("host-managed"));
        assert!(
            warnings[0]
                .message
                .contains("no backup or purge is scheduled")
        );
        assert!(cfg.cron.is_empty());
        assert!(
            cfg.prop_paths()
                .iter()
                .all(|path| !path.starts_with(&format!("{section}.")))
        );
        assert!(toml::Value::try_from(cfg).unwrap().get(section).is_none());
    }
}

#[test]
fn v3_retired_overlapping_tool_settings_warn_and_leave_no_tool_config() {
    for section in [
        "pipeline",
        "image_gen",
        "file_upload",
        "file_upload_bundle",
        "file_download",
    ] {
        let raw = format!("schema_version = 3\n[{section}]\nenabled = true\n");
        let cfg = parse_current(&raw).expect("removed tool capability has a config tombstone");
        let warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
        assert_eq!(warnings.len(), 1, "{section}");
        assert_eq!(warnings[0].code, "overlapping_builtin_tool_removed");
        assert_eq!(warnings[0].path, section);
        assert!(
            cfg.prop_paths()
                .iter()
                .all(|path| !path.starts_with(&format!("{section}.")))
        );
        assert!(toml::Value::try_from(cfg).unwrap().get(section).is_none());
    }
}

#[test]
fn v3_retired_prometheus_backend_warns_and_preserves_log_persistence() {
    for backend in ["prometheus", "PROMETHEUS"] {
        let raw = format!(
            "schema_version = 3\n[observability]\nbackend = \"{backend}\"\nlog_persistence = \"full\"\n"
        );
        let cfg = parse_current(&raw).expect("retired telemetry backend still loads");
        assert_eq!(
            cfg.observability.backend,
            reiclaw_config::schema::ObservabilityBackend::None
        );
        assert_eq!(
            cfg.observability.log_persistence,
            reiclaw_config::schema::LogPersistence::Full
        );
        let warnings = reiclaw_config::validation_warnings::retired_field_tombstones(&raw);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "prometheus_observability_removed");
        assert_eq!(warnings[0].path, "observability.backend");
        assert!(warnings[0].message.contains("/api/events"));
        let written = toml::to_string(&cfg).unwrap();
        assert!(!written.to_ascii_lowercase().contains("prometheus"));
    }
}

#[test]
fn v3_retired_memory_and_browser_backends_load_with_tombstone_warnings() {
    // postgres/qdrant/sqlite/lucid memory (plus snapshot_* / sqlite timeout)
    // and the rust_native browser backend were removed. A config that still
    // selects them must keep loading and report every retired selector;
    // selecting them then fails explicitly at construction (memory factory /
    // browser tool), never silently.
    let mut value: toml::Value =
        toml::from_str(&generate(&GenerateOptions::default()).expect("generate current succeeds"))
            .expect("generated V3 parses");
    let root = value.as_table_mut().unwrap();
    let retired: toml::Table = toml::from_str(
        r#"
[memory]
backend = "postgres.work"
sqlite_open_timeout_secs = 30
snapshot_enabled = true
snapshot_on_hygiene = true
auto_hydrate = false

[storage.postgres.work]
db_url = "postgres://user:pw@host/db"
vector_enabled = true

[storage.qdrant.default]
url = "http://localhost:6333"

[storage.sqlite.default]
open_timeout_secs = 30

[storage.lucid.default]
binary_path = "/opt/lucid/bin/lucid"

[browser]
backend = "rust_native"
native_headless = true
native_webdriver_url = "http://127.0.0.1:9515"
native_chrome_path = "/usr/bin/chromium"
"#,
    )
    .unwrap();
    for (key, table) in retired {
        let slot = root
            .entry(key)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap();
        for (k, v) in table.as_table().unwrap() {
            match (slot.get_mut(k).and_then(toml::Value::as_table_mut), v) {
                (Some(existing), toml::Value::Table(inner)) => existing.extend(inner.clone()),
                _ => {
                    slot.insert(k.clone(), v.clone());
                }
            }
        }
    }
    let raw = toml::to_string(&value).unwrap();

    let cfg = parse_current(&raw).expect("retired backends must not fail load");
    assert_eq!(cfg.memory.backend, "postgres.work");

    let mut warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
    warnings.extend(reiclaw_config::validation_warnings::retired_field_tombstones(&raw));
    let mut hits: Vec<(&str, &str)> = warnings
        .iter()
        .map(|w| (w.path.as_str(), w.code.as_str()))
        .collect();
    hits.sort_unstable();
    assert_eq!(
        hits,
        vec![
            ("browser", "browser_tools_removed"),
            ("browser.backend", "browser_native_backend_removed"),
            (
                "browser.native_chrome_path",
                "browser_native_backend_removed"
            ),
            ("browser.native_headless", "browser_native_backend_removed"),
            (
                "browser.native_webdriver_url",
                "browser_native_backend_removed"
            ),
            ("memory.auto_hydrate", "memory_snapshot_removed"),
            ("memory.backend", "memory_backend_removed"),
            ("memory.snapshot_enabled", "memory_snapshot_removed"),
            ("memory.snapshot_on_hygiene", "memory_snapshot_removed"),
            ("memory.sqlite_open_timeout_secs", "memory_backend_removed"),
            ("storage.lucid", "memory_backend_removed"),
            ("storage.postgres", "memory_backend_removed"),
            ("storage.qdrant", "memory_backend_removed"),
            ("storage.sqlite", "memory_backend_removed"),
        ],
        "{warnings:?}"
    );

    for legacy_backend in ["sqlite", "lucid"] {
        let mut backend_value = value.clone();
        backend_value["memory"]["backend"] = toml::Value::String(legacy_backend.to_string());
        let raw_legacy = toml::to_string(&backend_value).unwrap();
        let legacy_cfg = parse_current(&raw_legacy).expect("legacy backend still loads");
        assert_eq!(legacy_cfg.memory.backend, legacy_backend);
        let field_warnings =
            reiclaw_config::validation_warnings::retired_field_tombstones(&raw_legacy);
        assert!(
            field_warnings
                .iter()
                .any(|w| w.path == "memory.backend" && w.code == "memory_backend_removed"),
            "expected memory_backend_removed for {legacy_backend}, got {field_warnings:?}"
        );
    }
}

fn load_with_retired_section(path: &str, body: &str) -> (String, Vec<ValidationWarning>) {
    let mut value: toml::Value =
        toml::from_str(&generate(&GenerateOptions::default()).expect("generate current succeeds"))
            .expect("generated V3 parses");
    let mut table = value.as_table_mut().unwrap();
    let segments: Vec<&str> = path.split('.').collect();
    let (last, parents) = segments.split_last().unwrap();
    for segment in parents {
        table = table
            .entry(segment.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap();
    }
    table.insert((*last).to_string(), toml::from_str(body).unwrap());
    let raw = toml::to_string(&value).unwrap();

    let cfg = parse_current(&raw)
        .unwrap_or_else(|e| panic!("retired [{path}] section must not fail load: {e}"));
    cfg.validate()
        .unwrap_or_else(|e| panic!("retired [{path}] section must not fail validation: {e}"));
    let mut warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
    warnings.extend(reiclaw_config::validation_warnings::retired_field_tombstones(&raw));
    (raw, warnings)
}

#[test]
fn retired_voice_duplex_does_not_activate_or_grant_another_surface() {
    let (stamp, root, alias) = ("schema_version = 3\n", "channels", ".default");
    let path = format!("{root}.voice_duplex");
    let raw = format!(
        "{stamp}[{path}{alias}]\nenabled = true\nexcluded_tools = ['shell', 'file_write']\n\
         [{root}.webhook{alias}]\nenabled = true\nport = 8080\n"
    );
    let config = parse_current(&raw).expect("retired voice config must load");
    assert_eq!(config.channels.webhook["default"].port, 8080);
    assert!(config.peer_groups.is_empty());
    assert!(config.channels.is_retired_channel("voice_duplex"));
    assert!(!config.channels.is_known_channel("voice_duplex"));
    assert!(!config.channels.is_channel_deliverable("voice_duplex"));
    assert!(
        config
            .channels_by_alias()
            .iter()
            .all(|channel| channel.channel_type != "voice_duplex")
    );
    assert!(
        !toml::to_string(&config)
            .unwrap()
            .contains("[channels.voice_duplex")
    );
    let warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].path, path);
    assert_eq!(warnings[0].code, "retired_channel_transport_removed");
}

#[test]
fn generate_current_parses_and_validates() {
    let target = CURRENT_SCHEMA_VERSION;
    let raw = generate(&GenerateOptions::default())
        .unwrap_or_else(|e| panic!("generate({target}) failed: {e:#}"));
    let cfg = parse_current(&raw).unwrap_or_else(|e| {
        panic!("generate({target}) output failed to parse as current schema: {e:#}")
    });
    cfg.validate()
        .unwrap_or_else(|e| panic!("generate({target}) output fails Config::validate: {e:#}"));
}

#[test]
fn generate_never_emits_retired_sop_config() {
    let dir = tempfile::tempdir().unwrap();
    let target = CURRENT_SCHEMA_VERSION;
    for encrypt_secrets in [false, true] {
        let raw = generate(&GenerateOptions {
            encrypt_secrets,
            secret_store_dir: Some(dir.path()),
        })
        .unwrap();
        let value: toml::Value = toml::from_str(&raw).unwrap();
        assert!(
            value.get("sop").is_none(),
            "generated V{target} contains SOP"
        );
        parse_current(&raw).expect("generated config must remain loadable");
    }
}

#[test]
fn generate_never_emits_retired_config_surfaces() {
    // A freshly generated config must not trigger the
    // running binary's own removal warning on first load. Asserted through
    // the real detector so every retired surface in the shared table is
    // covered without re-implementing the walk.
    let target = CURRENT_SCHEMA_VERSION;
    let raw = generate(&GenerateOptions::default())
        .unwrap_or_else(|e| panic!("generate({target}) failed: {e:#}"));
    // Pin WhatsApp independently of the tombstone registry: forgetting its
    // registration must still fail for all generator schema versions.
    let value: toml::Value = toml::from_str(&raw).unwrap();
    let root = "channels";
    let channels = value[root].as_table().unwrap();
    assert!(!channels.contains_key("whatsapp"));
    // Owner-retired 2026-11-18: the generator must never emit these, since
    // loading them is refused outright.
    assert!(!channels.contains_key("discord"));
    assert!(!channels.contains_key("email"));
    for retained in ["telegram", "webhook"] {
        assert!(
            channels.contains_key(retained),
            "generate({target}) must retain {retained}"
        );
    }
    assert!(
        value
            .get("transcription")
            .and_then(|transcription| transcription.get("transcribe_non_ptt_audio"))
            .is_none()
    );
    let mut warnings = reiclaw_config::validation_warnings::retired_section_tombstones(&raw);
    warnings.extend(reiclaw_config::validation_warnings::retired_field_tombstones(&raw));
    assert!(
        warnings.is_empty(),
        "generate({target}) must not emit retired config surfaces or fields: {warnings:?}"
    );
}

#[test]
fn generate_never_emits_retired_batch_b_channel_settings() {
    // Literal expectations also catch a missing tombstone registration; the
    // general detector-derived test cannot discriminate that omission.
    let target = CURRENT_SCHEMA_VERSION;
    let raw = generate(&GenerateOptions::default()).unwrap();
    let value: toml::Value = toml::from_str(&raw).unwrap();
    let root = "channels";
    let channels = value[root].as_table().unwrap();
    for kind in [
        "line", "dingtalk", "wecom", "wecom_ws", "wechat", "qq", "twitter", "mochat",
    ] {
        assert!(
            !channels.contains_key(kind),
            "generate({target}) must omit {kind}"
        );
    }
    assert!(!channels.contains_key("discord"));
    assert!(!channels.contains_key("email"));
}

#[test]
fn generate_never_emits_retired_batch_c_channel_settings() {
    let target = CURRENT_SCHEMA_VERSION;
    let raw = generate(&GenerateOptions::default()).unwrap();
    let value: toml::Value = toml::from_str(&raw).unwrap();
    let root = "channels";
    let channels = value[root].as_table().unwrap();
    for kind in [
        "nostr",
        "clawdtalk",
        "reddit",
        "bluesky",
        "git",
        "voice_call",
        "voice_wake",
        "amqp",
    ] {
        assert!(
            !channels.contains_key(kind),
            "generate({target}) must omit {kind}"
        );
    }
    assert!(!channels.contains_key("discord"));
    assert!(!channels.contains_key("email"));
}

#[test]
fn generate_current_emits_at_current_schema_version() {
    let raw = generate(&GenerateOptions::default()).expect("generate current succeeds");
    let parsed: toml::Value = toml::from_str(&raw).expect("generated output parses as TOML");
    assert_eq!(
        parsed
            .get("schema_version")
            .and_then(toml::Value::as_integer),
        Some(i64::from(CURRENT_SCHEMA_VERSION)),
        "generate(CURRENT) must stamp the current schema_version"
    );
}

#[test]
fn generate_v3_covers_every_v3_top_level_section() {
    // Pin the example's current capabilities and policy-bearing sections.
    let cfg =
        parse_current(&generate(&GenerateOptions::default()).expect("generate current succeeds"))
            .expect("generated current schema parses");

    assert!(
        cfg.agents.contains_key("simple_agent"),
        "agents.simple_agent present in the current example"
    );
    assert!(
        cfg.agents.contains_key("complex_agent"),
        "agents.complex_agent present in the current example"
    );
    assert!(
        cfg.risk_profiles.contains_key("default"),
        "risk profile is present"
    );
    assert!(
        cfg.runtime_profiles.contains_key("default"),
        "runtime profile is present"
    );
    assert!(
        cfg.cron.contains_key("morning_briefing"),
        "current example includes the named cron job"
    );
    assert!(cfg.scheduler.enabled, "scheduler remains enabled");
    assert!(
        !cfg.peer_groups.is_empty(),
        "retained channel allowlists remain in peer_groups"
    );
    // Comprehensive-fixture markers: each of these is a top-level V3
    // section the fixture covers. Drop one and the regression suite
    // surfaces it.
    assert!(cfg.gateway.require_pairing, "gateway block carried through");
    assert!(cfg.heartbeat.enabled, "heartbeat block carried through");
    assert!(cfg.web_search.enabled, "web_search block carried through");
}

#[test]
fn generate_v3_preserves_retained_channels_and_retires_transports() {
    // Pin the fixture's retained aliases and meaningful fields, rather than a
    // count another alias could accidentally satisfy after losing a channel.
    let generated = generate(&GenerateOptions::default()).expect("generate current succeeds");
    let cfg = parse_current(&generated).expect("generated V3 parses");
    let channels = serde_json::to_value(&cfg.channels).expect("serialize channel configuration");
    let expected_fields = serde_json::json!({
        "telegram": {
            "enabled": true,
            "bot_token": "telegram-v1-bot-token",
            "stream_mode": "partial",
            "draft_update_interval_ms": 1000,
            "interrupt_on_new_message": false,
            "mention_only": false,
            "ack_reactions": false
        },
        "webhook": {
            "enabled": true,
            "port": 8080,
            "listen_path": "/webhook",
            "send_url": "https://hooks.example.com/inbound",
            "send_method": "POST"
        }
    });
    let expected_fields = expected_fields.as_object().unwrap();
    let expected_aliases: std::collections::BTreeSet<_> = expected_fields
        .keys()
        .map(|kind| format!("{kind}.default"))
        .collect();
    let mut actual_aliases = std::collections::BTreeSet::new();
    for (kind, value) in channels.as_object().unwrap() {
        if let Some(aliases) = value.as_object() {
            for alias in aliases.keys() {
                actual_aliases.insert(format!("{kind}.{alias}"));
            }
        }
    }
    assert_eq!(
        actual_aliases, expected_aliases,
        "retained channel aliases must remain exact"
    );
    for (kind, fields) in expected_fields {
        for (field, expected) in fields.as_object().unwrap() {
            assert_eq!(
                channels[kind]["default"][field], *expected,
                "channels.{kind}.default.{field} must preserve the fixture's configuration"
            );
        }
    }

    // Pin every retained peer and its scope: dropping or widening an
    // allowlist must fail even when the example still parses.
    let expected_peers = serde_json::json!({
        "telegram": {
            "agents": ["complex_agent"],
            "external_peers": ["@v1_admin", "12345678"]
        }
    });
    let expected_peers = expected_peers.as_object().unwrap();
    let expected_groups: std::collections::BTreeSet<_> = expected_peers
        .keys()
        .map(|kind| format!("{kind}_default"))
        .collect();
    assert_eq!(
        cfg.peer_groups
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        expected_groups,
        "only retained allowlists may appear in peer groups"
    );
    let expected_agent_channels: std::collections::BTreeSet<_> = expected_peers
        .keys()
        .map(|kind| format!("{kind}.default"))
        .collect();
    assert_eq!(
        cfg.agents["complex_agent"]
            .channels
            .iter()
            .map(ToString::to_string)
            .collect::<std::collections::BTreeSet<_>>(),
        expected_agent_channels,
        "the example owner must bind exactly the peer groups' retained aliases"
    );
    assert!(cfg.agents["simple_agent"].channels.is_empty());
    for (kind, expected) in expected_peers {
        let name = format!("{kind}_default");
        let group = serde_json::to_value(&cfg.peer_groups[&name]).unwrap();
        assert_eq!(
            group["channel"],
            serde_json::json!(kind),
            "{name} channel scope"
        );
        assert_eq!(group["agents"], expected["agents"], "{name} agent scope");
        assert_eq!(
            group["external_peers"], expected["external_peers"],
            "{name} complete allowlist"
        );
        assert_eq!(group["ignore"], serde_json::json!([]), "{name} blocklist");
        assert_eq!(
            group["admin_for_agent_scope"], false,
            "{name} must not grant admin"
        );
    }

    for kind in [
        "line",
        "dingtalk",
        "wecom",
        "wecom_ws",
        "wechat",
        "qq",
        "twitter",
        "mochat",
        "nostr",
        "clawdtalk",
        "reddit",
        "bluesky",
        "git",
        "voice_call",
        "voice_wake",
        "amqp",
        "whatsapp",
    ] {
        assert!(
            channels.get(kind).is_none(),
            "retired {kind} must have no alias"
        );
        assert!(!cfg.peer_groups.contains_key(&format!("{kind}_default")));
    }

    // A retired section in a current config must warn without exposing
    // credentials or carrying restrictions onto another transport.
    let retired = r#"schema_version = 3
[channels.whatsapp.default]
access_token = "retired-access-token"
app_secret = "retired-app-secret"
verify_token = "retired-verify-token"
allowed_users = ["retired-private-peer"]
"#;
    assert!(channels.get("whatsapp").is_none());
    assert!(!cfg.peer_groups.contains_key("whatsapp_default"));
    let warnings = reiclaw_config::validation_warnings::retired_section_tombstones(retired);
    let whatsapp_warnings: Vec<_> = warnings
        .iter()
        .filter(|warning| warning.path == "channels.whatsapp")
        .collect();
    assert_eq!(
        whatsapp_warnings.len(),
        1,
        "legacy WhatsApp must warn explicitly"
    );
    let warning = whatsapp_warnings[0];
    assert_eq!(warning.code, "retired_channel_transport_removed");
    assert!(warning.message.contains("no runtime consumer"));
    assert!(warning.message.contains("Remove the section."));
    let fixture: toml::Value = toml::from_str(retired).unwrap();
    for field in ["access_token", "app_secret", "verify_token"] {
        let secret = fixture["channels"]["whatsapp"]["default"][field]
            .as_str()
            .unwrap();
        assert!(
            !warning.message.contains(secret),
            "retirement must not expose {field}"
        );
    }
}

#[test]
fn generate_with_encrypt_produces_enc2_ciphertext() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SecretStore::new(tmp.path(), true);

    let target = CURRENT_SCHEMA_VERSION;
    let raw = generate(&GenerateOptions {
        encrypt_secrets: true,
        secret_store_dir: Some(tmp.path()),
    })
    .unwrap_or_else(|e| panic!("generate({target}) --encrypt failed: {e:#}"));

    // The output must contain enc2: ciphertext (at least one secret
    // was encrypted) and must not contain any of the well-known
    // plaintext fixture secrets.
    assert!(
        raw.contains("enc2:"),
        "generate({target}) --encrypt produced no enc2: ciphertext"
    );
    for plaintext in &["sk-v1-test-global", "telegram-v1-bot-token"] {
        assert!(
            !raw.contains(plaintext),
            "generate({target}) --encrypt leaked plaintext secret {plaintext:?}"
        );
    }

    // Round-trip a known leaf back through decrypt to prove the
    // ciphertext is real (not just a literal `enc2:` prefix).
    let parsed: toml::Value = toml::from_str(&raw).expect("encrypted output parses");
    let api_key_ciphertext = find_first_string_at_key(&parsed, "api_key")
        .unwrap_or_else(|| panic!("generate({target}) has no api_key leaf to decrypt"));
    assert!(
        api_key_ciphertext.starts_with("enc2:"),
        "generate({target}) api_key leaf must be enc2: ciphertext, got {api_key_ciphertext:?}"
    );
    let plaintext = store
        .decrypt(&api_key_ciphertext)
        .unwrap_or_else(|e| panic!("generate({target}) api_key failed to decrypt: {e:#}"));
    let plain: toml::Value =
        toml::from_str(&generate(&GenerateOptions::default()).unwrap()).unwrap();
    assert_eq!(Some(plaintext), find_first_string_at_key(&plain, "api_key"));
}

fn find_first_string_at_key(value: &toml::Value, key: &str) -> Option<String> {
    match value {
        toml::Value::Table(t) => {
            if let Some(toml::Value::String(s)) = t.get(key) {
                return Some(s.clone());
            }
            for child in t.values() {
                if let Some(found) = find_first_string_at_key(child, key) {
                    return Some(found);
                }
            }
            None
        }
        toml::Value::Array(items) => items.iter().find_map(|v| find_first_string_at_key(v, key)),
        _ => None,
    }
}

#[test]
fn encryption_covers_every_schema_secret_field() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = generate(&GenerateOptions {
        encrypt_secrets: true,
        secret_store_dir: Some(tmp.path()),
    })
    .expect("encrypted generate succeeds");
    let encrypted: toml::Value = toml::from_str(&raw).expect("encrypted output parses");
    let plaintext_cfg = parse_current(CURRENT_FIXTURE).expect("native current fixture parses");

    let mut missed = Vec::new();
    for field in plaintext_cfg
        .prop_fields()
        .into_iter()
        .filter(|f| f.is_secret)
    {
        let snake_path: String = field
            .name
            .split('.')
            .map(|seg| seg.replace('-', "_"))
            .collect::<Vec<_>>()
            .join(".");
        let leaf = lookup_dotted(&encrypted, &snake_path);
        match leaf {
            Some(toml::Value::String(s))
                if !s.is_empty() && !s.starts_with("enc2:") && !s.starts_with("enc:") =>
            {
                missed.push(format!("{snake_path} = {s:?}"));
            }
            Some(toml::Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    if let toml::Value::String(s) = item
                        && !s.is_empty()
                        && !s.starts_with("enc2:")
                        && !s.starts_with("enc:")
                    {
                        missed.push(format!("{snake_path}[{i}] = {s:?}"));
                    }
                }
            }
            _ => {}
        }
    }
    assert!(
        missed.is_empty(),
        "schema-derived encrypt walker missed these #[secret] paths in \
         the generated config — the field exists in prop_fields() but its \
         string leaf survived as plaintext:\n\n{}",
        missed.join("\n")
    );
}

#[test]
fn encryption_covers_compound_map_secret_field() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SecretStore::new(tmp.path(), true);

    let raw_toml = r#"
schema_version = 3

[[mcp.servers]]
name = "primary"
transport = "sse"
url = "https://mcp.example.invalid/sse"
command = ""

[mcp.servers.headers]
Authorization = "Bearer mcp-cred"
X-Tenant = "tenant-42"
"#;

    let mut value: toml::Value = toml::from_str(raw_toml).expect("toml parses");
    encrypt_secret_strings(&mut value, &store).expect("encrypt walker succeeds");

    let server = value
        .get("mcp")
        .and_then(|v| v.get("servers"))
        .and_then(toml::Value::as_array)
        .and_then(|arr| arr.first())
        .expect("mcp.servers[0] table");
    let headers = server
        .get("headers")
        .and_then(toml::Value::as_table)
        .expect("mcp.servers[0].headers table");

    for (key, val) in headers {
        let s = val
            .as_str()
            .unwrap_or_else(|| panic!("headers.{key} is not a string"));
        assert!(
            s.starts_with("enc2:"),
            "mcp.servers[0].headers.{key} must be enc2-prefixed; got: {s}"
        );
    }
    let auth = headers
        .get("Authorization")
        .and_then(toml::Value::as_str)
        .expect("Authorization value");
    let tenant = headers
        .get("X-Tenant")
        .and_then(toml::Value::as_str)
        .expect("X-Tenant value");
    assert_eq!(
        store.decrypt(auth).expect("decrypt auth"),
        "Bearer mcp-cred",
    );
    assert_eq!(store.decrypt(tenant).expect("decrypt tenant"), "tenant-42",);

    assert_eq!(
        server.get("url").and_then(toml::Value::as_str),
        Some("https://mcp.example.invalid/sse"),
    );
    assert_eq!(
        server.get("name").and_then(toml::Value::as_str),
        Some("primary"),
    );
}

#[test]
fn encryption_preserves_onepassword_secret_references() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SecretStore::new(tmp.path(), true);

    let raw_toml = r#"
schema_version = 3

[providers.models.openai.default]
model = "gpt-5"
api_key = "op://zeroclaw/provider/openai-api-key"
"#;

    let mut value: toml::Value = toml::from_str(raw_toml).expect("toml parses");
    encrypt_secret_strings(&mut value, &store).expect("encrypt walker succeeds");

    let api_key = lookup_dotted(&value, "providers.models.openai.default.api_key")
        .and_then(toml::Value::as_str);

    assert_eq!(api_key, Some("op://zeroclaw/provider/openai-api-key"));
}

#[test]
fn encryption_preserves_tachi_vault_secret_references() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SecretStore::new(tmp.path(), true);

    let raw_toml = r#"
schema_version = 3

[providers.models.openai.default]
model = "gpt-5"
api_key = "vault:OPENAI_OFFICIAL_API_KEY"
"#;

    let mut value: toml::Value = toml::from_str(raw_toml).expect("toml parses");
    encrypt_secret_strings(&mut value, &store).expect("encrypt walker succeeds");

    let api_key = lookup_dotted(&value, "providers.models.openai.default.api_key")
        .and_then(toml::Value::as_str);

    assert_eq!(api_key, Some("vault:OPENAI_OFFICIAL_API_KEY"));
}

fn lookup_dotted<'a>(value: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    let mut cur = value;
    for segment in path.split('.') {
        let table = cur.as_table()?;
        cur = table.get(segment)?;
    }
    Some(cur)
}

#[test]
fn get_prop_resolves_model_field_for_typed_provider_alias() {
    use reiclaw_config::schema::Config;
    let raw = r#"
schema_version = 3

[providers.models.anthropic.glados]
model = "claude-opus-4-7"
max_tokens = 25000

[providers.models.anthropic.clamps]
model = "claude-sonnet-4-6"
"#;
    let cfg: Config = toml::from_str(raw).expect("parse");
    let glados_model = cfg
        .get_prop("providers.models.anthropic.glados.model")
        .expect("get_prop must resolve for typed provider alias .model field");
    let clamps_model = cfg
        .get_prop("providers.models.anthropic.clamps.model")
        .expect("get_prop must resolve for the other alias too");
    eprintln!("glados.model = {glados_model:?}");
    eprintln!("clamps.model = {clamps_model:?}");
    assert_eq!(glados_model, "claude-opus-4-7");
    assert_eq!(clamps_model, "claude-sonnet-4-6");
}

#[test]
fn prop_fields_includes_providers_models_alias_model_path() {
    // The gateway's /api/config/prop handler does lookup_prop_field(&config, &path).
    // lookup_prop_field walks Config::prop_fields() and finds a matching entry.
    // If the model path isn't in prop_fields(), the gateway returns 404 and
    // the frontend's resolveModelToProviderType walk silently drops the alias.
    use reiclaw_config::schema::Config;
    let raw = r#"
schema_version = 3

[providers.models.anthropic.glados]
model = "claude-opus-4-7"
"#;
    let cfg: Config = toml::from_str(raw).expect("parse");
    let want = "providers.models.anthropic.glados.model";
    let all = cfg.prop_fields();
    let found = all.iter().any(|f| f.name == want);
    if !found {
        let candidates: Vec<String> = all
            .iter()
            .filter(|f| f.name.contains("anthropic.glados"))
            .map(|f| f.name.clone())
            .collect();
        panic!("prop_fields() missing `{want}` — candidates: {candidates:?}",);
    }
}

#[test]
fn typed_family_root_is_not_a_map_keyed_section() {
    use reiclaw_config::schema::Config;
    let raw = r#"
schema_version = 3
[providers.models.anthropic.glados]
model = "claude-opus-4-7"
"#;
    let cfg: Config = toml::from_str(raw).expect("parse");
    assert!(
        cfg.get_map_keys("providers.models").is_none(),
        "typed-family root must NOT resolve as a single map — typed wrapper has per-type HashMap fields",
    );
    assert_eq!(
        cfg.get_map_keys("providers.models.anthropic"),
        Some(vec!["glados".to_string()]),
        "the per-type slot IS a map keyed by alias",
    );
}

#[test]
fn map_key_sections_exposes_typed_family_slots() {
    // Regression: the dashboard's walkConfiguredModelBindings now reads
    // the slot list from /api/config/templates (backed by
    // Config::map_key_sections()). Verify every providers.<category>
    // typed-family root has its per-type slots registered there.
    use reiclaw_config::schema::Config;
    let paths: std::collections::HashSet<&'static str> = Config::map_key_sections()
        .iter()
        .filter(|s| matches!(s.kind, reiclaw_config::traits::MapKeyKind::Map))
        .map(|s| s.path)
        .collect();
    for required in [
        "providers.models.anthropic",
        "providers.models.openai",
        "providers.tts.openai",
        "providers.transcription.openai",
    ] {
        assert!(
            paths.contains(required),
            "map_key_sections() must expose `{required}` — frontend depends on this via /api/config/templates",
        );
    }
}

#[test]
fn unsupported_generations_never_salvage_to_current_defaults() {
    for version in [
        None,
        Some("0"),
        Some("1"),
        Some("2"),
        Some("4"),
        Some("-1"),
        Some("4294967299"),
        Some("\"3\""),
    ] {
        let stamp = version
            .map(|v| format!("schema_version = {v}\n"))
            .unwrap_or_default();
        let raw =
            format!("{stamp}[gateway]\nrequire_pairing = true\n[security]\naudit = \"invalid\"\n");
        let strict = parse_current(&raw).unwrap_err();
        let recovery = parse_current_salvaged(&raw).unwrap_err();
        assert!(strict.is::<reiclaw_config::current::UnsupportedSchemaError>());
        assert!(recovery.is::<reiclaw_config::current::UnsupportedSchemaError>());
    }
    assert_eq!(
        detect_version(&toml::from_str("schema_version = 3").unwrap()).unwrap(),
        3
    );
}

#[test]
fn minimal_current_input_matches_current_defaults_without_legacy_synthesis() {
    let parsed = parse_current("schema_version = 3\n").unwrap();
    let defaults = Config::default();
    assert_eq!(
        toml::to_string(&parsed).unwrap(),
        toml::to_string(&defaults).unwrap()
    );
    assert!(parsed.agents.is_empty());
    assert!(parsed.providers.models.is_empty());
    assert!(parsed.risk_profiles.is_empty());
    assert!(parsed.runtime_profiles.is_empty());
}

#[test]
fn disk_version_gate_is_read_only_and_accepts_current_or_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    ensure_disk_at_current_version(&path).unwrap();
    for raw in [
        "",
        "schema_version = 1\n",
        "schema_version = 2\n",
        "schema_version = 4\n",
        "schema_version = 4294967299\n",
    ] {
        std::fs::write(&path, raw).unwrap();
        assert!(ensure_disk_at_current_version(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    std::fs::write(&path, "schema_version = 3\n").unwrap();
    ensure_disk_at_current_version(&path).unwrap();
}

#[tokio::test]
async fn all_save_modes_refuse_unsupported_disk_versions_before_secret_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut config = Config {
        config_path: path.clone(),
        loaded_from: Some(path.clone()),
        ..Config::default()
    };
    config
        .set_prop_persistent("http_request.secrets.api_token", "private-pending-token")
        .unwrap();
    for raw in [
        "",
        "schema_version = 1\n",
        "schema_version = 2\n",
        "schema_version = 4\n",
    ] {
        for mode in ["save", "force_save", "save_dirty"] {
            std::fs::write(&path, raw).unwrap();
            let error = match mode {
                "save" => Box::pin(config.save()).await,
                "force_save" => Box::pin(config.force_save()).await,
                _ => Box::pin(config.save_dirty()).await,
            }
            .unwrap_err();
            assert!(
                error.is::<reiclaw_config::current::UnsupportedSchemaError>(),
                "{mode}: {error:#}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
            assert!(
                config
                    .dirty_paths
                    .contains("http_request.secrets.api_token")
            );
        }
    }
}

#[test]
fn retired_sop_and_provider_credentials_still_block_current_parsing_and_recovery() {
    for section in [
        "[sop]\nstep_scope_enforce = true",
        "[providers.models.openrouter.private_alias]\napi_key = 'private-retired-key'",
    ] {
        let raw = format!("schema_version = 3\n{section}\n");
        for error in [
            parse_current(&raw).unwrap_err(),
            parse_current_salvaged(&raw).unwrap_err(),
        ] {
            let message = format!("{error:#}");
            assert!(message.contains("retired"));
            assert!(!message.contains("private-retired-key"));
            assert!(!message.contains("private_alias"));
        }
    }
}

#[test]
fn property_and_json_input_cannot_change_the_supported_version() {
    let mut config = Config::default();
    for version in [0, 1, 2, 4] {
        assert!(
            config
                .set_prop_persistent("schema_version", &version.to_string())
                .is_err()
        );
        assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(config.dirty_paths.is_empty());
        assert!(
            serde_json::from_value::<Config>(serde_json::json!({"schema_version": version}))
                .is_err()
        );
    }
    config.set_prop_persistent("gateway.port", "42618").unwrap();
    assert_eq!(config.gateway.port, 42618);
    assert_eq!(
        toml::from_str::<Config>("").unwrap().schema_version,
        CURRENT_SCHEMA_VERSION
    );
}

#[test]
fn legacy_root_fields_are_diagnosed_as_unknown_in_current_input() {
    let unknown = Config::unknown_keys(
        "schema_version = 3\napi_key = 'private-unused-key'\ndefault_provider = 'openai'\nlocale = 'en'\n",
    );
    assert_eq!(unknown, ["api_key", "default_provider"]);
}

#[test]
fn development_templates_are_native_current_config_with_explicit_policy() {
    for raw in [
        include_str!("../../../../dev/config.template.toml"),
        include_str!("../../../../dev/config.harness-test.toml"),
    ] {
        let config = parse_current(raw).unwrap();
        assert!(Config::unknown_keys(raw).is_empty());
        assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
        let provider = &config.providers.models.ollama["default"].base;
        assert_eq!(
            provider.uri.as_deref(),
            Some("http://host.docker.internal:11434")
        );
        assert!(provider.api_key.is_none());
        assert_eq!(provider.model.as_deref(), Some("llama3.2"));
        assert_eq!(
            config.agents["default"].model_provider.as_str(),
            "ollama.default"
        );
        assert_eq!(config.agents["default"].risk_profile, "default");
        assert_eq!(config.agents["default"].runtime_profile, "default");
        assert_eq!(
            config.risk_profiles["default"].level,
            reiclaw_config::autonomy::AutonomyLevel::Supervised
        );
    }
    let harness = parse_current(include_str!("../../../../dev/config.harness-test.toml")).unwrap();
    assert_eq!(harness.runtime_profiles["default"].max_tool_iterations, 50);
    assert_eq!(
        harness.runtime_profiles["default"].max_tool_result_chars,
        Some(50_000)
    );
    assert!(
        harness.risk_profiles["default"]
            .auto_approve
            .contains(&"file_write".to_string())
    );
}
