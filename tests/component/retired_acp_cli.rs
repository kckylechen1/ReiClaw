//! The retired `rei acp` command must fail by naming the real replacement
//! binary, not a command that does not exist.

use std::path::Path;
use std::process::Output;
use tokio::process::Command;

const CONFIG: &str = "schema_version = 3\n";

fn fixture(contents: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("isolated config directory");
    std::fs::write(dir.path().join("config.toml"), contents).expect("write config");
    dir
}

async fn run_cli(dir: &Path, args: &[&str]) -> Output {
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
        .arg("--config-dir")
        .arg(dir)
        .args(args)
        .output()
        .await
        .expect("run rei")
}

#[tokio::test]
async fn retired_acp_names_the_bridge_binary() {
    let dir = fixture(CONFIG);
    let output = run_cli(dir.path(), &["acp"]).await;
    assert!(
        !output.status.success(),
        "`rei acp` must keep failing after retirement"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rei-acp-bridge"),
        "the retirement error must name the real replacement binary: {stderr}"
    );
    assert!(
        !stderr.contains("acp-gateway"),
        "the retirement error must not advertise a nonexistent command: {stderr}"
    );
}
