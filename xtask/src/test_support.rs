//! Helpers that read feature and distribution policy directly from the manifest.

use std::path::PathBuf;

use crate::util::repo_root;

#[track_caller]
pub fn require_non_empty(subject: &str, len: usize) {
    assert!(len > 0, "{subject} must contain a real assertion sample");
}

fn manifest() -> PathBuf {
    repo_root().join("Cargo.toml")
}

fn cargo_toml() -> toml::Value {
    let path = manifest();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    toml::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

fn reiclaw_metadata() -> toml::Value {
    cargo_toml()
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("reiclaw"))
        .cloned()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()))
}

fn string_list(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// One string list from `[package.metadata.reiclaw]`, parsed straight out of
/// Cargo.toml. Tests compare production's resolved value against the *file*,
/// not against the same in-memory value production already resolved.
pub fn registry_string_list(key: &str) -> Vec<String> {
    string_list(reiclaw_metadata().get(key))
}

/// `[package.metadata.reiclaw.dist_target_exclusions]`, parsed straight out of
/// Cargo.toml.
pub fn registry_target_exclusions() -> std::collections::BTreeMap<String, Vec<String>> {
    reiclaw_metadata()
        .get("dist_target_exclusions")
        .and_then(|v| v.as_table())
        .map(|table| {
            table
                .iter()
                .map(|(target, value)| (target.clone(), string_list(Some(value))))
                .collect()
        })
        .unwrap_or_default()
}

/// Every feature name in the root package `[features]` table (Cargo's own
/// resolver, same source the spec reads).
pub fn live_feature_names() -> Vec<String> {
    let meta = cargo_metadata::MetadataCommand::new()
        .manifest_path(manifest())
        .no_deps()
        .exec()
        .expect("cargo metadata for the workspace root");
    let pkg = meta
        .root_package()
        .cloned()
        .or_else(|| meta.workspace_packages().into_iter().next().cloned())
        .expect("root package");
    pkg.features.keys().cloned().collect()
}
