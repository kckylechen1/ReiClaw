//! Full and incremental persistence share secret preparation, not write policy.

use reiclaw_config::current::parse_current;
use reiclaw_config::schema::Config;
use reiclaw_config::secrets::SecretStore;
use tempfile::TempDir;

const ENV_PATH: &str = "http_request.secrets.environment";
const EDITED_PATH: &str = "http_request.secrets.edited";
const EXTERNAL_PATH: &str = "http_request.secrets.external";
const EXTERNAL_REF: &str = "op://fixture/item/token";

fn save_fixture() -> (TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let store = SecretStore::new(dir.path(), true);
    let environment = store.encrypt("stored-environment-secret").unwrap();
    let edited = store.encrypt("stored-edited-secret").unwrap();
    let untouched = store.encrypt("stored-untouched-secret").unwrap();
    let raw = format!(
        r#"schema_version = 3

[gateway]
host = "127.0.0.1"
port = 42617

# Credentials retained through an unrelated edit.
[http_request.secrets]
environment = "{environment}"
edited = "{edited}"
external = "{EXTERNAL_REF}"
untouched = "{untouched}" # retain this ciphertext and comment on dirty save

[future_extension]
mode = "preserved by incremental writes"
"#
    );
    std::fs::write(&path, &raw).unwrap();
    let mut config = parse_current(&raw).unwrap();
    config.config_path = path.clone();
    config.loaded_from = Some(path);
    config.data_dir = dir.path().join("data");

    // Reproduce the loaded runtime state without an external secret process:
    // plaintext values serve the runtime, snapshots retain the on-disk inputs.
    for (key, value) in [
        ("environment", "runtime-environment-secret"),
        ("edited", "runtime-edited-secret"),
        ("external", "resolved-external-secret"),
        ("untouched", "stored-untouched-secret"),
    ] {
        config.http_request.secrets.insert(key.into(), value.into());
    }
    for (path, original) in [
        (ENV_PATH, "stored-environment-secret"),
        (EDITED_PATH, "stored-edited-secret"),
    ] {
        config.env_overridden_paths.insert(path.into());
        config
            .pre_override_snapshots
            .insert(path.into(), original.into());
    }
    config
        .onepassword_reference_snapshots
        .insert(EXTERNAL_PATH.into(), EXTERNAL_REF.into());
    config
        .set_prop_persistent(EDITED_PATH, "explicit-edited-secret")
        .unwrap();
    config.set_prop_persistent("gateway.port", "42618").unwrap();
    (dir, config)
}

async fn persist(config: &mut Config, incremental: bool) -> anyhow::Result<()> {
    if incremental {
        Box::pin(config.save_dirty()).await
    } else {
        Box::pin(config.save()).await
    }
}

#[tokio::test]
async fn full_and_dirty_saves_share_secret_rules_and_keep_distinct_document_policy() {
    for incremental in [false, true] {
        let (dir, mut config) = save_fixture();
        let original = std::fs::read_to_string(&config.config_path).unwrap();
        let original_value: toml::Value = toml::from_str(&original).unwrap();
        persist(&mut config, incremental).await.unwrap();

        let saved = std::fs::read_to_string(&config.config_path).unwrap();
        let value: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(value["schema_version"].as_integer(), Some(3));
        assert_eq!(value["gateway"]["port"].as_integer(), Some(42618));
        let secrets = value["http_request"]["secrets"].as_table().unwrap();
        let store = SecretStore::new(dir.path(), true);
        for (key, expected) in [
            ("environment", "stored-environment-secret"),
            ("edited", "explicit-edited-secret"),
            ("untouched", "stored-untouched-secret"),
        ] {
            let ciphertext = secrets[key].as_str().unwrap();
            assert!(ciphertext.starts_with("enc2:"));
            assert_eq!(store.decrypt(ciphertext).unwrap(), expected);
            assert!(!saved.contains(expected));
        }
        assert_eq!(secrets["external"].as_str(), Some(EXTERNAL_REF));
        assert!(!saved.contains("runtime-environment-secret"));
        assert!(!saved.contains("runtime-edited-secret"));
        assert!(!saved.contains("resolved-external-secret"));

        assert_eq!(
            config.http_request.secrets["environment"],
            "runtime-environment-secret"
        );
        assert_eq!(
            config.http_request.secrets["edited"],
            "explicit-edited-secret"
        );
        assert_eq!(
            config.http_request.secrets["external"],
            "resolved-external-secret"
        );
        assert!(config.env_overridden_paths.contains(ENV_PATH));
        assert!(config.pre_override_snapshots.contains_key(ENV_PATH));
        assert!(!config.env_overridden_paths.contains(EDITED_PATH));
        assert!(!config.pre_override_snapshots.contains_key(EDITED_PATH));
        assert_eq!(
            config
                .onepassword_reference_snapshots
                .get(EXTERNAL_PATH)
                .as_deref(),
            Some(EXTERNAL_REF)
        );

        if incremental {
            assert_eq!(
                secrets["untouched"],
                original_value["http_request"]["secrets"]["untouched"]
            );
            assert!(saved.contains("# retain this ciphertext and comment on dirty save"));
            assert!(value.get("future_extension").is_some());
            assert!(value["gateway"].get("host").is_some());
            assert!(config.dirty_paths.is_empty());
        } else {
            assert!(value.get("future_extension").is_none());
            assert!(value["gateway"].get("host").is_none());
            assert!(config.dirty_paths.contains(EDITED_PATH));
        }
    }
}

#[tokio::test]
async fn full_and_dirty_write_failures_preserve_pending_state_for_retry() {
    for incremental in [false, true] {
        let (_dir, mut config) = save_fixture();
        let before = config.clone();
        let original = std::fs::read(&config.config_path).unwrap();
        // Fail inside the real atomic writer after all preparation, without
        // depending on chmod semantics or a synthetic write implementation.
        let backup_path = config.config_path.with_extension("toml.bak");
        std::fs::create_dir(&backup_path).unwrap();
        let error = persist(&mut config, incremental).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Failed to create config backup before atomic replace")
        );
        assert_eq!(std::fs::read(&config.config_path).unwrap(), original);
        assert_eq!(
            toml::to_string(&config).unwrap(),
            toml::to_string(&before).unwrap()
        );
        assert_eq!(config.loaded_from, before.loaded_from);
        assert_eq!(config.dirty_paths, before.dirty_paths);
        assert_eq!(config.dirty_map_entries, before.dirty_map_entries);
        assert_eq!(
            config.explicit_prop_dirty_paths,
            before.explicit_prop_dirty_paths
        );
        assert_eq!(
            config.tracked_prop_identities,
            before.tracked_prop_identities
        );
        assert_eq!(config.env_overridden_paths, before.env_overridden_paths);
        assert_eq!(config.pre_override_snapshots, before.pre_override_snapshots);
        assert_eq!(
            config.onepassword_reference_snapshots,
            before.onepassword_reference_snapshots
        );

        std::fs::remove_dir(&backup_path).unwrap();
        persist(&mut config, incremental).await.unwrap();
        let saved = std::fs::read_to_string(&config.config_path).unwrap();
        let value: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(value["gateway"]["port"].as_integer(), Some(42618));
        assert_eq!(
            value["http_request"]["secrets"]["external"].as_str(),
            Some(EXTERNAL_REF)
        );
        assert!(!config.env_overridden_paths.contains(EDITED_PATH));
        assert!(!config.pre_override_snapshots.contains_key(EDITED_PATH));
        assert!(!backup_path.exists());
    }
}
