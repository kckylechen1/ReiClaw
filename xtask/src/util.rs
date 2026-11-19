use std::path::{Path, PathBuf};
use std::process::Command;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below repo root")
        .to_owned()
}

pub fn require_tool(cmd: &str, install_hint: &str) -> anyhow::Result<()> {
    if tool_on_path(cmd) {
        return Ok(());
    }
    anyhow::bail!("'{}' not found on PATH\n  install: {}", cmd, install_hint);
}

fn tool_on_path(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join(cmd).is_file() || dir.join(format!("{cmd}.exe")).is_file())
        })
        .unwrap_or(false)
}

pub fn run_cmd(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("command failed: {:?}", cmd.get_program());
    }
    Ok(())
}
