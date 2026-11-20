//! Exercise the actual CLI boundary, including configuration load ordering.

use std::path::Path;
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;

const CONFIG: &str = r#"schema_version = 3
[providers.models.openai.broken]
api_key = "vault:ESTOP_TEST_MISSING_KEY"
model = "test-model"
[security.estop]
enabled = true
state_file = "control/estop.json"
require_otp_to_resume = true
[security.otp]
enabled = true
[secrets]
encrypt = false
"#;

fn fixture(contents: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("isolated config directory");
    std::fs::write(dir.path().join("config.toml"), contents).expect("write config");
    std::fs::create_dir(dir.path().join("bin")).expect("empty CLI search path");
    dir
}

async fn run_cli(dir: &Path, args: &[&str], overrides: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rei"));
    // Do not let the developer's configured instance or env policy enter fixtures.
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|name| name.starts_with("REI_") || name.starts_with("REICLAW_"))
        {
            command.env_remove(key);
        }
    }
    command
        .kill_on_drop(true)
        .env("REI_CONFIG_DIR", dir)
        .env("PATH", dir.join("bin"))
        .env("RUST_LOG", "off")
        .envs(overrides.iter().copied())
        .args(args);
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("local estop command must return promptly")
        .expect("execute real CLI")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn state(dir: &Path, relative: &str) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join(relative)).expect("persisted state"))
        .expect("state JSON")
}

#[tokio::test]
async fn engage_and_status_bypass_missing_vault_without_rewriting_configuration() {
    let dir = fixture(CONFIG);
    let overrides = [(
        "REI_providers__models__openai__broken__api_key",
        "vault:UNAVAILABLE",
    )];
    assert_success(&run_cli(dir.path(), &["estop"], &overrides).await);
    assert!(
        state(dir.path(), "control/estop.json")["kill_all"]
            .as_bool()
            .unwrap()
    );
    let live_handle = reiclaw_runtime::security::estop::EstopHandle::for_path(
        dir.path().join("control/estop.json"),
    );
    assert!(live_handle.enforce_not_killed().is_err());
    let before = std::fs::read(dir.path().join("control/estop.json")).unwrap();
    assert_success(&run_cli(dir.path(), &["estop", "status"], &overrides).await);
    assert_eq!(
        std::fs::read(dir.path().join("control/estop.json")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
        CONFIG
    );
    for path in ["otp-secret", ".secret_key", "data", "shared"] {
        assert!(
            !dir.path().join(path).exists(),
            "estop created unrelated {path}"
        );
    }

    // The same broken credential still fails the normal full-configuration path.
    let normal = run_cli(dir.path(), &["status"], &[]).await;
    assert!(!normal.status.success());
    assert!(String::from_utf8_lossy(&normal.stderr).contains("Tachi CLI (`tachi`) not found"));
}

#[cfg(unix)]
#[tokio::test]
async fn engage_does_not_invoke_a_failing_vault_cli() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fixture(CONFIG);
    let stub = dir.path().join("bin/tachi");
    std::fs::write(
        &stub,
        "#!/bin/sh\nprintf called > \"$REI_CONFIG_DIR/vault-called\"\nexit 17\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_success(&run_cli(dir.path(), &["estop"], &[]).await);
    assert_success(&run_cli(dir.path(), &["estop", "status"], &[]).await);
    assert!(!dir.path().join("vault-called").exists());
    assert!(!run_cli(dir.path(), &["status"], &[]).await.status.success());
    assert!(
        dir.path().join("vault-called").exists(),
        "sentinel must detect ordinary credential loading"
    );
}

#[tokio::test]
async fn local_safety_env_overrides_and_cli_config_directory_are_honored() {
    let ignored = fixture(CONFIG);
    let selected = fixture(&CONFIG.replace("enabled = true", "enabled = false"));
    let config_dir = selected.path().to_str().unwrap();
    let overrides = [
        ("REI_security__estop__enabled", "true"),
        ("REI_security__estop__state_file", "override/stop.json"),
        ("REI_security__estop__require_otp_to_resume", "true"),
        ("REI_security__otp__enabled", "true"),
    ];
    assert_success(
        &run_cli(
            ignored.path(),
            &["--config-dir", config_dir, "estop"],
            &overrides,
        )
        .await,
    );
    assert!(
        state(selected.path(), "override/stop.json")["kill_all"]
            .as_bool()
            .unwrap()
    );
    assert!(!selected.path().join("control/estop.json").exists());
    assert!(!ignored.path().join("control").exists());
    let resume = run_cli(
        ignored.path(),
        &[
            "--config-dir",
            config_dir,
            "estop",
            "resume",
            "--otp",
            "invalid",
        ],
        &overrides,
    )
    .await;
    assert!(!resume.status.success());
    assert!(
        state(selected.path(), "override/stop.json")["kill_all"]
            .as_bool()
            .unwrap()
    );
}

#[tokio::test]
async fn resume_requires_existing_valid_otp_and_preserves_engaged_state_on_failure() {
    let dir = fixture(CONFIG);
    assert_success(&run_cli(dir.path(), &["estop"], &[]).await);
    let before = std::fs::read(dir.path().join("control/estop.json")).unwrap();
    let resume = ["estop", "resume", "--otp", "invalid"];
    assert!(!run_cli(dir.path(), &resume, &[]).await.status.success());
    assert!(!dir.path().join("otp-secret").exists());
    assert_eq!(
        std::fs::read(dir.path().join("control/estop.json")).unwrap(),
        before
    );

    assert_success(&run_cli(dir.path(), &["estop", "otp-init"], &[]).await);
    assert!(dir.path().join("otp-secret").exists());
    assert!(!run_cli(dir.path(), &resume, &[]).await.status.success());
    std::fs::write(dir.path().join("otp-secret"), "!corrupt!").unwrap();
    assert!(!run_cli(dir.path(), &resume, &[]).await.status.success());
    assert_eq!(
        std::fs::read(dir.path().join("control/estop.json")).unwrap(),
        before
    );

    // An explicit local policy disabling OTP remains effective; no secret is
    // read or repaired, and only the existing stop state is changed.
    assert_success(
        &run_cli(
            dir.path(),
            &["estop", "resume"],
            &[("REI_security__estop__require_otp_to_resume", "false")],
        )
        .await,
    );
    assert!(
        !state(dir.path(), "control/estop.json")["kill_all"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("otp-secret")).unwrap(),
        "!corrupt!"
    );
}

#[tokio::test]
async fn malformed_safety_settings_are_refused_without_changing_stop_state() {
    let dir = fixture(CONFIG);
    assert_success(&run_cli(dir.path(), &["estop"], &[]).await);
    let before = std::fs::read(dir.path().join("control/estop.json")).unwrap();
    let broken_configs = [
        CONFIG.replace(
            "require_otp_to_resume = true",
            "require_otp_to_resume = 'no'",
        ),
        CONFIG.replace("state_file = \"control/estop.json\"", "state_file = ''"),
        CONFIG.replace(
            "[security.otp]\nenabled = true",
            "[security.otp]\nenabled = true\ntoken_ttl_secs = 0",
        ),
        CONFIG.replace("[security.otp]", "[security.otp]\nunknown_setting = true"),
        CONFIG.replace("schema_version = 3", "schema_version = 2"),
        "schema_version = 3\nsecurity = false\n".to_owned(),
        "schema_version = 3\n[security".to_owned(),
    ];
    for contents in broken_configs {
        std::fs::write(dir.path().join("config.toml"), &contents).unwrap();
        for args in [
            &["estop"][..],
            &["estop", "status"],
            &["estop", "resume", "--otp", "invalid"],
        ] {
            let output = run_cli(dir.path(), args, &[]).await;
            assert!(
                !output.status.success(),
                "malformed safety config succeeded for {args:?}"
            );
            assert!(!String::from_utf8_lossy(&output.stderr).contains("Tachi CLI"));
            assert_eq!(
                std::fs::read(dir.path().join("control/estop.json")).unwrap(),
                before
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
                contents
            );
        }
    }
    std::fs::write(dir.path().join("config.toml"), CONFIG).unwrap();
    let output = run_cli(
        dir.path(),
        &["estop"],
        &[("REI_security__estop__enabled", "invalid")],
    )
    .await;
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(dir.path().join("control/estop.json")).unwrap(),
        before
    );
}
