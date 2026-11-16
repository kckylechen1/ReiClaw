//! Configuration persistence, dirty-path tracking, and atomic TOML writes.
//!
//! The schema owns declarations. This module retains the existing save ordering:
//! prepare a separate candidate, validate and write, then publish saved state.

use crate::load::{default_config_and_data_dirs, resolve_runtime_config_dirs};
use crate::schema::{Config, ExternalRefSnapshots, TrackedPropIdentity};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use tokio::fs::File;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

/// Top-level keys that must always appear in the saved config even
/// when their value equals the default. `schema_version` is the
/// migration detector's anchor — dropping it from a freshly-saved
/// config would make the next load mis-detect the file as V1 (no
/// version key = V1).
const SAVE_PRESERVE_KEYS: &[&str] = &["schema_version"];

/// Insert a blank line before every `[section]` header that doesn't
/// already have one, so the serialized TOML reads as discrete blocks
/// instead of running every section header directly after the
/// previous line (`toml::to_string_pretty` doesn't gap between a
/// trailing scalar and the next section header).
fn ensure_blank_line_before_sections(toml: &str) -> String {
    let mut out = String::with_capacity(toml.len() + 64);
    let mut prev_line_blank = true; // start of file counts as blank
    for line in toml.lines() {
        let is_section_header = line.starts_with('[');
        if is_section_header && !prev_line_blank {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
        prev_line_blank = line.trim().is_empty();
    }
    out
}

/// Walk `actual` and drop every key whose value matches the same
/// key's value in `defaults`. Tables recurse; the recursion drops a
/// sub-table when every one of its keys was itself dropped (i.e. the
/// sub-table contained only defaults). Keys that don't appear in
/// `defaults` are operator-added and always survive.
///
/// HashMap-keyed sub-trees (e.g. `agents`, `providers.models.<family>`)
/// are not in the typed default tree, so their operator-added aliases
/// pass through this filter unchanged.
fn prune_default_values(actual: &mut toml::Table, defaults: &toml::Table) {
    let keys: Vec<String> = actual.keys().cloned().collect();
    for key in keys {
        if SAVE_PRESERVE_KEYS.contains(&key.as_str()) {
            continue;
        }
        let Some(default_value) = defaults.get(&key) else {
            // Operator added this key; not in the typed default tree.
            // Always keep — recursing in would either be a no-op or
            // strip operator content.
            continue;
        };
        let Some(child) = actual.remove(&key) else {
            continue;
        };
        let pruned = match (child, default_value) {
            (toml::Value::Table(mut child_table), toml::Value::Table(default_subtable)) => {
                prune_default_values(&mut child_table, default_subtable);
                if child_table.is_empty() {
                    None
                } else {
                    Some(toml::Value::Table(child_table))
                }
            }
            (child, default_value) => {
                if &child == default_value {
                    None
                } else {
                    Some(child)
                }
            }
        };
        if let Some(value) = pruned {
            actual.insert(key, value);
        }
    }
}

impl Config {
    pub fn mark_dirty(&mut self, path: &str) {
        self.dirty_paths.insert(path.to_string());
        if !self.tracked_prop_identities.contains_key(path)
            && !self.is_known_whole_entry_path(path)
            && let Some(enc) = self.encode_prop_snapshot_key(None, path)
        {
            let had_sibling_entry = self.has_live_entry_at_raw_path(path);
            self.tracked_prop_identities.insert(
                path.to_string(),
                TrackedPropIdentity {
                    encoded_key: enc,
                    had_sibling_entry,
                },
            );
            self.explicit_prop_dirty_paths.insert(path.to_string());
        }
    }

    fn has_live_entry_at_raw_path(&self, path: &str) -> bool {
        if let Some(section) = find_natural_key_section_for_path(path) {
            let rest = &path[section.section_path.len() + 1..];
            return self
                .get_map_keys(section.section_path)
                .is_some_and(|keys| keys.iter().any(|k| k == rest));
        }
        if let Some(section) = find_map_key_section_for_path(path) {
            let rest = &path[section.path.len() + 1..];
            return self
                .get_map_keys(section.path)
                .is_some_and(|keys| keys.iter().any(|k| k == rest));
        }
        false
    }

    fn has_dirty_map_entry_at_raw_path(&self, path: &str) -> bool {
        if let Some(section) = find_natural_key_section_for_path(path) {
            let rest = &path[section.section_path.len() + 1..];
            return self
                .dirty_map_entries
                .contains(&format!("{}\0{rest}", section.section_path));
        }
        if let Some(section) = find_map_key_section_for_path(path) {
            let rest = &path[section.path.len() + 1..];
            return self
                .dirty_map_entries
                .contains(&format!("{}\0{rest}", section.path));
        }
        false
    }

    fn is_known_whole_entry_path(&self, path: &str) -> bool {
        let normalized = path.replace('-', "_");
        if Self::map_key_sections()
            .iter()
            .any(|s| s.path == path || s.path == normalized)
        {
            return true;
        }
        self.has_dirty_map_entry_at_raw_path(path) || self.has_live_entry_at_raw_path(path)
    }

    /// Auto-create the parent map key for a dotted set-prop `path` when it does
    /// not yet exist, so a `set_prop` on a brand-new alias's field materializes
    /// the entry first.
    ///
    /// Returns `true` iff it REFUSED to create the reserved `default` agent: a
    /// set-prop surface should then surface a reserved-name error rather than the
    /// generic "not configured" that the still-missing entry would otherwise
    /// produce. Returns `false` in every other case (created, already existed, or
    /// the path is not a map-keyed entry). The bool is advisory; statement-callers
    /// that do not distinguish the reserved case may ignore it.
    ///
    /// `#[resource_key]` sections are excluded. Their keys are values drawn from
    /// another domain (model id, voice, tool name) and may themselves contain
    /// dots, so the first-dot split below would read
    /// `cost.rates.providers.models.openai.gpt-4.1.input_per_mtok` as the bogus
    /// alias `gpt-4` plus a nonsense tail, and `create_map_key` skips
    /// `validate_alias_key` for those sections so nothing else rejects it.
    /// Rate rows are created explicitly through `POST /api/config/map-key`
    /// instead.
    pub fn ensure_map_key_for_path(&mut self, path: &str) -> bool {
        use crate::traits::MapKeyKind;
        let mut best: Option<&'static str> = None;
        for s in Self::map_key_sections()
            .iter()
            .filter(|s| s.kind == MapKeyKind::Map)
            .filter(|s| !s.resource_key)
        {
            let prefix = format!("{}.", s.path);
            if path.starts_with(&prefix)
                && path.len() > prefix.len()
                && best.is_none_or(|b| s.path.len() > b.len())
            {
                best = Some(s.path);
            }
        }
        let Some(section) = best else {
            return false;
        };
        let rest = &path[section.len() + 1..];
        let Some(alias) = rest.split('.').next().filter(|a| !a.is_empty()) else {
            return false;
        };
        if self
            .get_map_keys(section)
            .is_some_and(|keys| keys.iter().any(|k| k == alias))
        {
            return false;
        }
        // Never auto-vivify the reserved `default` agent from a set-prop path: a
        // prop write under a nonexistent `agents.default` must not materialize the
        // reserved runtime fallback (the operator would get an agent the rename
        // guard then traps). This is the set-prop analogue of the create guard in
        // `create_map_key_checked`. The migration that legitimately synthesizes
        // `agents.default` calls `create_map_key` directly, not this path, and an
        // already-present `default` is left untouched by the existence check above.
        if section == "agents" && crate::alias_refs::is_reserved_agent_alias(alias) {
            // Make the refusal observable: callers that check the return surface a
            // reserved-name error, and this WARN with a stable `error_key` lets an
            // operator see why a set-prop on a fresh `agents.default` was dropped.
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::zeroclaw_log::Action::Reject
                )
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "error_key": "config.reserved_agent_vivify_refused",
                    "alias": alias,
                    "path": path,
                })),
                "refused to auto-create the reserved `default` agent from a set-prop path"
            );
            return true;
        }
        let _ = self.create_map_key(section, alias);
        false
    }

    pub fn clear_dirty(&mut self) {
        self.dirty_paths.clear();
        self.dirty_map_entries.clear();
        self.explicit_prop_dirty_paths.clear();
        let env_paths = &self.env_overridden_paths;
        let pre_snaps = &self.pre_override_snapshots;
        self.tracked_prop_identities
            .retain(|k, _| env_paths.contains(k) || pre_snaps.contains_key(k));
    }

    pub(crate) fn bind_env_override_identities(&mut self) {
        let paths: Vec<String> = self
            .env_overridden_paths
            .iter()
            .chain(self.pre_override_snapshots.keys())
            .collect();
        for path in paths {
            if let Some(enc) = self.encode_prop_snapshot_key(None, &path) {
                let had_sibling_entry = self.has_live_entry_at_raw_path(&path);
                self.tracked_prop_identities.insert(
                    path,
                    TrackedPropIdentity {
                        encoded_key: enc,
                        had_sibling_entry,
                    },
                );
            }
        }
    }

    pub(crate) fn verify_tracked_prop_identities(
        &self,
        resolved_dirty_props: Option<&std::collections::HashSet<String>>,
    ) -> std::result::Result<(), String> {
        if self.tracked_prop_identities.is_empty() {
            return Ok(());
        }
        let computed_dirty_props;
        let active_dirty_props = match resolved_dirty_props {
            Some(props) => props,
            None => {
                let whole = self.effective_whole_entry_dirty_entries(None);
                computed_dirty_props = self.collect_dirty_prop_paths(&whole);
                &computed_dirty_props
            }
        };
        let mut keys: Vec<&String> = self.tracked_prop_identities.keys().collect();
        keys.sort();
        for raw_path in keys {
            let is_active = self.dirty_paths.contains(raw_path)
                || active_dirty_props.contains(raw_path)
                || self.explicit_prop_dirty_paths.contains(raw_path)
                || self.env_overridden_paths.contains(raw_path)
                || self.pre_override_snapshots.contains_key(raw_path);
            if !is_active {
                continue;
            }
            let expected = &self.tracked_prop_identities[raw_path];
            let actual_enc = self.encode_prop_snapshot_key(None, raw_path);
            if actual_enc.as_deref() != Some(expected.encoded_key.as_str()) {
                return Err(format!(
                    "Refusing config operation: tracked property `{raw_path}` rebound from \
                     `{}` to `{}`; persist or clear pending edits before introducing \
                     overlapping dotted keys",
                    expected.encoded_key,
                    actual_enc.as_deref().unwrap_or("<unresolvable>")
                ));
            }
            if !expected.had_sibling_entry
                && (self.has_live_entry_at_raw_path(raw_path)
                    || self.has_dirty_map_entry_at_raw_path(raw_path))
            {
                return Err(format!(
                    "Refusing config operation: tracked property `{raw_path}` collides with a \
                     newly introduced section entry of the same dotted path; persist or clear \
                     pending edits first"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn record_dirty_map_entry(&mut self, section_path: &str, key: &str) {
        self.dirty_map_entries
            .insert(format!("{section_path}\0{key}"));
    }

    fn effective_whole_entry_dirty_entries(
        &self,
        on_disk_table: Option<&toml::Table>,
    ) -> Vec<(String, String)> {
        let mut set: std::collections::BTreeSet<(String, String)> =
            std::collections::BTreeSet::new();
        for encoded in &self.dirty_map_entries {
            if let Some((section, key)) = encoded.split_once('\0') {
                let candidate_dirty = format!("{section}.{key}");
                if self.dirty_paths.contains(&candidate_dirty) {
                    set.insert((section.to_string(), key.to_string()));
                }
            }
        }
        for path in &self.dirty_paths {
            if self.explicit_prop_dirty_paths.contains(path) {
                continue;
            }
            if let Some(section) = find_natural_key_section_for_path(path) {
                let rest = &path[section.section_path.len() + 1..];
                let in_mem = self
                    .get_map_keys(section.section_path)
                    .is_some_and(|keys| keys.iter().any(|k| k == rest));
                let on_disk = on_disk_table.is_some_and(|disk| {
                    let segs: Vec<&str> = section.section_path.split('.').collect();
                    lookup_path_in_table(disk, &segs)
                        .and_then(toml::Value::as_array)
                        .is_some_and(|arr| {
                            arr.iter().any(|item| {
                                item.as_table()
                                    .and_then(|t| t.get(section.natural_key_field))
                                    .and_then(toml::Value::as_str)
                                    == Some(rest)
                            })
                        })
                });
                if in_mem || on_disk {
                    set.insert((section.section_path.to_string(), rest.to_string()));
                }
                continue;
            }
            if let Some(section) = find_map_key_section_for_path(path) {
                let rest = &path[section.path.len() + 1..];
                let in_mem = self
                    .get_map_keys(section.path)
                    .is_some_and(|keys| keys.iter().any(|k| k == rest));
                let on_disk = on_disk_table.is_some_and(|disk| {
                    let segs: Vec<&str> = section.path.split('.').collect();
                    lookup_path_in_table(disk, &segs)
                        .and_then(toml::Value::as_table)
                        .is_some_and(|t| t.contains_key(rest))
                });
                if in_mem || on_disk {
                    set.insert((section.path.to_string(), rest.to_string()));
                }
            }
        }
        set.into_iter().collect()
    }

    fn collect_dirty_prop_paths(
        &self,
        whole_entries: &[(String, String)],
    ) -> std::collections::HashSet<String> {
        self.dirty_paths
            .iter()
            .filter(|path| {
                self.explicit_prop_dirty_paths.contains(*path)
                    || !whole_entries.iter().any(|(section, key)| {
                        path.len() == section.len() + 1 + key.len()
                            && path.starts_with(section.as_str())
                            && path.as_bytes().get(section.len()) == Some(&b'.')
                            && &path[section.len() + 1..] == key.as_str()
                    })
            })
            .cloned()
            .collect()
    }

    pub fn set_prop_persistent(&mut self, name: &str, value_str: &str) -> Result<()> {
        if let Some(expected) = self.tracked_prop_identities.get(name)
            && self.encode_prop_snapshot_key(None, name).as_deref()
                != Some(expected.encoded_key.as_str())
        {
            anyhow::bail!(
                "Refusing to mutate `{name}`: property path rebound from `{}` \
                 after a map/list key change",
                expected.encoded_key
            );
        }
        let prior_had_sibling = self
            .tracked_prop_identities
            .get(name)
            .map(|id| id.had_sibling_entry);
        self.set_prop(name, value_str)?;
        self.explicit_prop_dirty_paths.insert(name.to_string());
        self.dirty_paths.insert(name.to_string());
        if let Some(enc) = self.encode_prop_snapshot_key(None, name) {
            let had_sibling_entry =
                prior_had_sibling.unwrap_or_else(|| self.has_live_entry_at_raw_path(name));
            self.tracked_prop_identities.insert(
                name.to_string(),
                TrackedPropIdentity {
                    encoded_key: enc,
                    had_sibling_entry,
                },
            );
        }
        Ok(())
    }

    pub fn set_secret_persistent(&mut self, name: &str, value: String) -> Result<()> {
        if let Some(expected) = self.tracked_prop_identities.get(name)
            && self.encode_prop_snapshot_key(None, name).as_deref()
                != Some(expected.encoded_key.as_str())
        {
            anyhow::bail!(
                "Refusing to mutate `{name}`: secret path rebound from `{}` \
                 after a map/list key change",
                expected.encoded_key
            );
        }
        let prior_had_sibling = self
            .tracked_prop_identities
            .get(name)
            .map(|id| id.had_sibling_entry);
        self.set_secret(name, value)?;
        self.explicit_prop_dirty_paths.insert(name.to_string());
        self.dirty_paths.insert(name.to_string());
        if let Some(enc) = self.encode_prop_snapshot_key(None, name) {
            let had_sibling_entry =
                prior_had_sibling.unwrap_or_else(|| self.has_live_entry_at_raw_path(name));
            self.tracked_prop_identities.insert(
                name.to_string(),
                TrackedPropIdentity {
                    encoded_key: enc,
                    had_sibling_entry,
                },
            );
        }
        Ok(())
    }

    async fn resolve_config_path_for_save(&self) -> Result<PathBuf> {
        if self
            .config_path
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
        {
            return Ok(self.config_path.clone());
        }

        let (default_zeroclaw_dir, default_workspace_dir) = default_config_and_data_dirs()?;
        let (zeroclaw_dir, _workspace_dir, source) =
            resolve_runtime_config_dirs(&default_zeroclaw_dir, &default_workspace_dir).await?;
        let file_name = self
            .config_path
            .file_name()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("config.toml"));
        let resolved = zeroclaw_dir.join(file_name);
        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(crate::schema::CONFIG_SCHEMA_LOG_TARGET, ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"path": self.config_path.display().to_string(), "resolved": resolved.display().to_string(), "source": source.as_str()})), "Config path missing parent directory; resolving from runtime environment");
        Ok(resolved)
    }

    /// Full save. Refuses to overwrite an existing config file unless this
    /// value was loaded from that exact file (`loaded_from`), so a default,
    /// programmatically built, or repointed `Config` cannot replace an
    /// operator's config with a near-empty snapshot. Creating a
    /// missing file (first run) always succeeds — a value that never read
    /// a file gets exactly one create and must then reload or use
    /// `force_save()`; `force_save()` is the explicit overwrite path.
    pub async fn save(&self) -> Result<()> {
        self.save_impl(false, true).await.map(|_| ())
    }

    /// Same as `save()`, but skips the loaded-provenance guard. Only for
    /// callers that verifiably intend to replace an existing file with a
    /// `Config` that never read it.
    pub async fn force_save(&self) -> Result<()> {
        self.save_impl(true, true).await.map(|_| ())
    }

    async fn save_impl(
        &self,
        force: bool,
        validate_dirty_refs: bool,
    ) -> Result<(PathBuf, std::collections::HashMap<String, String>)> {
        // Encrypt secrets before serialization
        let mut config_to_save = self.clone();
        // Stamp the current schema version on every write. The in-memory
        // config is always at `CURRENT_SCHEMA_VERSION` (load-time migration
        // brings it forward), but pin it explicitly so a full save can never
        // emit a body-newer-than-label file. See `save_dirty` and.
        config_to_save.schema_version = crate::migration::CURRENT_SCHEMA_VERSION;
        let config_path = self.resolve_config_path_for_save().await?;
        // Fail closed: a metadata error must not read as "file absent" and
        // slip an unproven save past the guard.
        let target_exists = config_path.try_exists().with_context(|| {
            format!(
                "Cannot check config target {}; refusing to save",
                config_path.display()
            )
        })?;
        if !force && target_exists && self.loaded_from.as_ref() != Some(&config_path) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::zeroclaw_log::Action::Reject
                )
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "error_key": "config.save_refused_unproven_overwrite",
                    "path": config_path.display().to_string(),
                })),
                "refusing to overwrite an existing config this Config was never loaded from"
            );
            anyhow::bail!(
                "Refusing to overwrite existing config at {}: this Config was \
                 not loaded from that file (default-constructed, built \
                 programmatically, or loaded from a different file), so saving \
                 would replace the operator's file with a near-empty snapshot. \
                 Load it first (e.g. `Config::load_or_init`) or call \
                 `force_save()` to overwrite deliberately.",
                config_path.display()
            );
        }
        let zeroclaw_dir = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let existing_raw = if target_exists {
            Some(fs::read_to_string(&config_path).await.with_context(|| {
                format!(
                    "Failed to read existing config before save: {}; refusing to overwrite unchecked configuration",
                    config_path.display()
                )
            })?)
        } else {
            None
        };
        let on_disk_table: Option<toml::Table> = existing_raw
            .as_deref()
            .and_then(|s| toml::from_str::<toml::Table>(s).ok());
        if let Some(table) = on_disk_table.as_ref() {
            crate::migration::reject_retired_sop_config(table)?;
        }
        let store = crate::secrets::SecretStore::new(zeroclaw_dir, self.secrets.encrypt);
        let whole_entries = self.effective_whole_entry_dirty_entries(on_disk_table.as_ref());
        let dirty_prop_paths = self.collect_dirty_prop_paths(&whole_entries);
        self.verify_tracked_prop_identities(Some(&dirty_prop_paths))
            .map_err(anyhow::Error::msg)?;
        let scope_prefixes = self.collect_dirty_scope_prefixes(&whole_entries, &dirty_prop_paths);
        let restorable_snapshots = self.pruned_snapshots_for_restore(&scope_prefixes, false);
        let dirty_enc_keys = self.dirty_encoded_prop_keys(&dirty_prop_paths);

        // Restore non-dirty env-overridden paths to their pre-override snapshots
        // before encryption, so values supplied via `ZEROCLAW_*` env vars never
        // reach disk while explicit dirty edits still persist their new value.
        if !self.pre_override_snapshots.is_empty() {
            let non_dirty_snapshots: std::collections::HashMap<String, String> = self
                .pre_override_snapshots
                .iter()
                .filter(|(path, _)| {
                    !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys)
                })
                .collect();
            if !non_dirty_snapshots.is_empty() {
                crate::env_overrides::mask_env_overrides_for_save(
                    &mut config_to_save,
                    &non_dirty_snapshots,
                )?;
            }
        }
        let mut after_mask_refs = std::collections::HashMap::new();
        config_to_save.collect_external_secret_refs(None, &mut after_mask_refs)?;
        let mut skip_encoded_dirty: std::collections::HashSet<String> = dirty_prop_paths
            .iter()
            .filter_map(|raw| config_to_save.encode_prop_snapshot_key(None, raw))
            .collect();
        for (enc_k, ref_val) in &after_mask_refs {
            if restorable_snapshots.get(enc_k) != Some(ref_val) {
                skip_encoded_dirty.insert(enc_k.clone());
            }
        }
        restore_onepassword_references_for_save(
            &mut config_to_save,
            &restorable_snapshots,
            &skip_encoded_dirty,
        )?;

        let mut structural_refs = std::collections::HashMap::new();
        config_to_save.collect_external_secret_refs(None, &mut structural_refs)?;

        if validate_dirty_refs {
            // `save(&self)` / `force_save(&self)` takes a shared receiver, so it
            // validates dirty or newly introduced external references fail-closed
            // before touching disk without mutating `self`; live runtime edits
            // publish through `save_dirty(&mut self)`.
            let _validated = self
                .prepare_dirty_secret_edits(&config_path, &whole_entries, &dirty_prop_paths, false)
                .await?;
            let mut seen_unvalidated = std::collections::BTreeMap::new();
            for (enc_k, ref_val) in after_mask_refs.iter().chain(structural_refs.iter()) {
                if restorable_snapshots.get(enc_k) != Some(ref_val) {
                    seen_unvalidated
                        .entry(enc_k.clone())
                        .or_insert_with(|| ref_val.clone());
                }
            }
            if !seen_unvalidated.is_empty() {
                let verify_store =
                    crate::secrets::SecretStore::new(zeroclaw_dir, self.secrets.encrypt);
                tokio::task::spawn_blocking(move || -> Result<()> {
                    for (enc_k, ref_val) in seen_unvalidated {
                        verify_store.decrypt(&ref_val).with_context(|| {
                            format!("Failed to resolve external secret at {enc_k}")
                        })?;
                    }
                    Ok(())
                })
                .await
                .context("Config secret reference validation task failed")??;
            }
        }

        // Encrypt all #[secret]-annotated fields via Configurable derive
        config_to_save.encrypt_secrets(&store)?;

        // Serialize, then prune fields whose values match
        // `Config::default()` so the on-disk config carries only the
        // operator's actual choices (no hundreds of lines of struct
        // defaults the operator never touched). The schema's
        // `#[serde(default = "...")]` annotations re-supply the
        // defaults on load, so the pruned file round-trips identically.
        let mut new_table: toml::Table = toml::Value::try_from(&config_to_save)
            .context("Failed to serialize config to TOML value")?
            .try_into()
            .context("Serialized config is not a TOML table")?;
        let default_table: toml::Table = toml::Value::try_from(Config::default())
            .ok()
            .and_then(|v| v.try_into().ok())
            .unwrap_or_default();
        prune_default_values(&mut new_table, &default_table);
        let new_toml = ensure_blank_line_before_sections(
            &toml::to_string_pretty(&new_table).context("Failed to serialize pruned config")?,
        );

        // If an existing config file is present, sync the new values onto it
        // to preserve comments and formatting. Otherwise, use the fresh serialization.
        let toml_str = if let Some(existing) = existing_raw {
            if existing.is_empty() {
                new_toml
            } else {
                let mut doc: toml_edit::DocumentMut = existing
                    .parse()
                    .context("Failed to parse existing config for comment preservation")?;
                crate::migration::sync_table(doc.as_table_mut(), &new_table);
                // sync_table preserves existing decor verbatim, so newly
                // inserted sections lack the blank-line gap before their
                // header until the post-processor runs.
                ensure_blank_line_before_sections(&doc.to_string())
            }
        } else {
            new_toml
        };

        write_config_atomically(&config_path, &toml_str).await?;
        if validate_dirty_refs {
            // Prune snapshots that were cleared or replaced on disk, or whose in-memory
            // value after env-masking already holds a literal external reference rather
            // than resolved plaintext. Synchronize env-override metadata for persisted
            // dirty properties too so later full saves never mask back stale plaintext.
            // Failed saves return early above without mutating state.
            self.onepassword_reference_snapshots.retain(|k, old_ref| {
                structural_refs.get(k) == Some(&*old_ref) && !after_mask_refs.contains_key(k)
            });
            self.pre_override_snapshots.retain(|path, _| {
                !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys)
            });
            self.env_overridden_paths
                .retain(|path| !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys));
        }
        // Report the path actually written so `save_dirty`'s create
        // fallback can record provenance without re-resolving (the
        // resolver re-reads the environment for parentless paths).
        Ok((config_path, structural_refs))
    }

    fn dirty_encoded_prop_keys(
        &self,
        dirty_prop_paths: &std::collections::HashSet<String>,
    ) -> std::collections::HashSet<String> {
        dirty_prop_paths
            .iter()
            .filter_map(|p| {
                self.tracked_prop_identities
                    .get(p.as_str())
                    .map(|id| id.encoded_key.clone())
                    .or_else(|| self.encode_prop_snapshot_key(None, p))
            })
            .collect()
    }

    fn is_env_path_dirty(
        &self,
        path: &str,
        dirty_prop_paths: &std::collections::HashSet<String>,
        dirty_enc_keys: &std::collections::HashSet<String>,
    ) -> bool {
        if dirty_prop_paths.contains(path) {
            return true;
        }
        if dirty_enc_keys.is_empty() {
            return false;
        }
        self.tracked_prop_identities
            .get(path)
            .map(|id| id.encoded_key.clone())
            .or_else(|| self.encode_prop_snapshot_key(None, path))
            .is_some_and(|enc| dirty_enc_keys.contains(&enc))
    }

    /// Rewrite secret reference and env-override snapshot keys when an entry in
    /// `section_path` moves from `old_key` to `new_key`, using longest-key
    /// matching against sibling keys so overlapping prefixes (`fs` vs `fs.extra`)
    /// never collide.
    pub(crate) fn rename_secret_snapshots_in_section(
        &mut self,
        section_path: &str,
        old_key: &str,
        new_key: &str,
    ) -> std::result::Result<(), String> {
        self.record_dirty_map_entry(section_path, old_key);
        self.record_dirty_map_entry(section_path, new_key);
        let enc_section = encode_section_snapshot_prefix(section_path);
        let enc_old = crate::helpers::join_snapshot_path(
            &enc_section,
            &crate::helpers::encode_snapshot_segment(old_key),
        );
        let enc_new = crate::helpers::join_snapshot_path(
            &enc_section,
            &crate::helpers::encode_snapshot_segment(new_key),
        );
        let enc_old_dot = format!("{enc_old}.");
        let enc_old_bracket = format!("{enc_old}[");
        let moved_refs: Vec<(String, String)> = self
            .onepassword_reference_snapshots
            .to_map()
            .into_keys()
            .filter_map(|k| {
                if k == enc_old {
                    Some((k, enc_new.clone()))
                } else if let Some(tail) = k.strip_prefix(&enc_old_dot) {
                    let new_k = format!("{enc_new}.{tail}");
                    Some((k, new_k))
                } else if let Some(tail) = k.strip_prefix(&enc_old_bracket) {
                    let new_k = format!("{enc_new}[{tail}");
                    Some((k, new_k))
                } else {
                    None
                }
            })
            .collect();
        let extracted_refs: Vec<(String, String)> = moved_refs
            .into_iter()
            .filter_map(|(old_k, new_k)| {
                self.onepassword_reference_snapshots
                    .remove(&old_k)
                    .map(|val| (new_k, val))
            })
            .collect();
        for (new_k, val) in extracted_refs {
            self.onepassword_reference_snapshots.insert(new_k, val);
        }

        let mut pre_rename_keys = self.get_map_keys(section_path).unwrap_or_default();
        pre_rename_keys.retain(|k| k != new_key);
        if !pre_rename_keys.iter().any(|k| k == old_key) {
            pre_rename_keys.push(old_key.to_string());
        }
        let old_entry_dot = format!("{section_path}.{old_key}.");
        let new_entry = format!("{section_path}.{new_key}");
        if self.explicit_prop_dirty_paths.contains(&new_entry) {
            return Err(format!(
                "Refusing rename `{old_key}` -> `{new_key}` in `{section_path}`: \
                 dirty property `{new_entry}` collides with the target entry path"
            ));
        }
        let prior_identities = self.tracked_prop_identities.clone();

        // Leaf property paths always carry `.<inner_tail>` after `<section_path>.<old_key>`.
        // Prefer canonical bound identities when recorded so a shorter sibling's leaf path
        // is never misclassified as belonging to `old_key`.
        let rewrite_leaf_prop_key = |key: &str| -> Option<String> {
            if let Some(bound) = prior_identities.get(key) {
                let bound_enc = &bound.encoded_key;
                let belongs_to_old = bound_enc == &enc_old
                    || bound_enc.starts_with(&enc_old_dot)
                    || bound_enc.starts_with(&enc_old_bracket);
                if !belongs_to_old {
                    return None;
                }
                let inner_tail = key.strip_prefix(&old_entry_dot)?;
                return Some(format!("{new_entry}.{inner_tail}"));
            }
            let (matched, inner_tail) = crate::helpers::route_hashmap_path(
                key,
                "",
                section_path,
                "",
                pre_rename_keys.iter().map(String::as_str),
            )?;
            if matched != old_key {
                return None;
            }
            Some(format!("{new_entry}.{inner_tail}"))
        };

        let moved_identities: Vec<(String, String, TrackedPropIdentity)> = self
            .tracked_prop_identities
            .iter()
            .filter_map(|(raw_k, bound)| {
                let bound_enc = &bound.encoded_key;
                let new_enc = if bound_enc == &enc_old {
                    enc_new.clone()
                } else if let Some(tail) = bound_enc.strip_prefix(&enc_old_dot) {
                    format!("{enc_new}.{tail}")
                } else if let Some(tail) = bound_enc.strip_prefix(&enc_old_bracket) {
                    format!("{enc_new}[{tail}")
                } else {
                    return None;
                };
                let inner_tail = raw_k.strip_prefix(&old_entry_dot)?;
                let new_raw = format!("{new_entry}.{inner_tail}");
                let had_sibling_entry = pre_rename_keys
                    .iter()
                    .any(|sk| sk != old_key && format!("{section_path}.{sk}") == new_raw);
                Some((
                    raw_k.clone(),
                    new_raw,
                    TrackedPropIdentity {
                        encoded_key: new_enc,
                        had_sibling_entry,
                    },
                ))
            })
            .collect();

        let moved_pre_snaps: Vec<(String, String)> = self
            .pre_override_snapshots
            .keys()
            .filter_map(|k| {
                let nk = rewrite_leaf_prop_key(&k)?;
                Some((k, nk))
            })
            .collect();
        let moved_env_paths: Vec<(String, String)> = self
            .env_overridden_paths
            .iter()
            .filter_map(|k| {
                let nk = rewrite_leaf_prop_key(&k)?;
                Some((k, nk))
            })
            .collect();
        let was_explicit_prop: std::collections::HashSet<String> = self
            .explicit_prop_dirty_paths
            .iter()
            .filter(|k| rewrite_leaf_prop_key(k).is_some())
            .cloned()
            .collect();
        let moved_explicit_props: Vec<(String, String)> = self
            .explicit_prop_dirty_paths
            .iter()
            .filter_map(|k| rewrite_leaf_prop_key(k).map(|nk| (k.clone(), nk)))
            .collect();
        let moved_dirty_props: Vec<(String, String, bool)> = self
            .dirty_paths
            .iter()
            .filter_map(|k| {
                let new_k = rewrite_leaf_prop_key(k)?;
                let rest = k
                    .strip_prefix(section_path)
                    .and_then(|s| s.strip_prefix('.'))?;
                let sibling_whole_entry_recorded = self
                    .dirty_map_entries
                    .contains(&format!("{section_path}\0{rest}"));
                let sibling_exists = pre_rename_keys.iter().any(|sk| sk == rest);
                if was_explicit_prop.contains(k)
                    || (!sibling_whole_entry_recorded && !sibling_exists)
                {
                    Some((k.clone(), new_k, !sibling_whole_entry_recorded))
                } else {
                    None
                }
            })
            .collect();

        for (old_raw, _, _) in &moved_identities {
            self.tracked_prop_identities.remove(old_raw);
        }
        for (_, new_raw, new_id) in moved_identities {
            if self.tracked_prop_identities.contains_key(&new_raw) {
                return Err(format!(
                    "Refusing rename `{old_key}` -> `{new_key}` in `{section_path}`: \
                     tracked property `{new_raw}` collides with an existing sibling property"
                ));
            }
            self.tracked_prop_identities.insert(new_raw, new_id);
        }

        let extracted_pre_snaps: Vec<(String, String)> = moved_pre_snaps
            .into_iter()
            .filter_map(|(old_k, new_k)| {
                self.pre_override_snapshots
                    .remove(&old_k)
                    .map(|val| (new_k, val))
            })
            .collect();
        for (new_k, val) in extracted_pre_snaps {
            if self.pre_override_snapshots.contains_key(&new_k) {
                return Err(format!(
                    "Refusing rename `{old_key}` -> `{new_key}` in `{section_path}`: \
                     env-override snapshot `{new_k}` collides with an existing sibling property"
                ));
            }
            self.pre_override_snapshots.insert(new_k, val);
        }

        for (old_k, _) in &moved_env_paths {
            self.env_overridden_paths.remove(old_k);
        }
        for (_, new_k) in moved_env_paths {
            if self.env_overridden_paths.contains(&new_k) {
                return Err(format!(
                    "Refusing rename `{old_key}` -> `{new_key}` in `{section_path}`: \
                     env-overridden path `{new_k}` collides with an existing sibling property"
                ));
            }
            self.env_overridden_paths.insert(new_k);
        }

        for (old_k, _) in &moved_explicit_props {
            self.explicit_prop_dirty_paths.remove(old_k);
        }
        for (_, new_k) in moved_explicit_props {
            if self.explicit_prop_dirty_paths.contains(&new_k) {
                return Err(format!(
                    "Refusing rename `{old_key}` -> `{new_key}` in `{section_path}`: \
                     dirty property `{new_k}` collides with an existing sibling property"
                ));
            }
            self.explicit_prop_dirty_paths.insert(new_k);
        }

        for (old_k, _, remove_old) in &moved_dirty_props {
            if *remove_old {
                self.dirty_paths.remove(old_k);
            }
        }
        for (_, new_k, _) in moved_dirty_props {
            self.dirty_paths.insert(new_k);
        }

        self.verify_tracked_prop_identities(None)
    }

    /// Remove secret reference and env-override snapshot keys under a deleted
    /// entry in `section_path`, preserving longer sibling keys (`fs` vs `fs.extra`).
    pub(crate) fn drop_secret_snapshots_in_section(
        &mut self,
        section_path: &str,
        deleted_key: &str,
    ) {
        self.record_dirty_map_entry(section_path, deleted_key);
        let enc_section = encode_section_snapshot_prefix(section_path);
        let enc_deleted = crate::helpers::join_snapshot_path(
            &enc_section,
            &crate::helpers::encode_snapshot_segment(deleted_key),
        );
        let enc_deleted_dot = format!("{enc_deleted}.");
        let enc_deleted_bracket = format!("{enc_deleted}[");
        self.onepassword_reference_snapshots.retain(|k, _| {
            k != &enc_deleted
                && !k.starts_with(&enc_deleted_dot)
                && !k.starts_with(&enc_deleted_bracket)
        });

        let mut pre_delete_keys = self.get_map_keys(section_path).unwrap_or_default();
        if !pre_delete_keys.iter().any(|k| k == deleted_key) {
            pre_delete_keys.push(deleted_key.to_string());
        }
        let prior_identities = self.tracked_prop_identities.clone();
        let matches_deleted_leaf_prop = |key: &str| -> bool {
            if let Some(bound) = prior_identities.get(key) {
                let bound_enc = &bound.encoded_key;
                return bound_enc == &enc_deleted
                    || bound_enc.starts_with(&enc_deleted_dot)
                    || bound_enc.starts_with(&enc_deleted_bracket);
            }
            crate::helpers::route_hashmap_path(
                key,
                "",
                section_path,
                "",
                pre_delete_keys.iter().map(String::as_str),
            )
            .is_some_and(|(matched, _)| matched == deleted_key)
        };
        self.pre_override_snapshots
            .retain(|k, _| !matches_deleted_leaf_prop(k));
        self.env_overridden_paths
            .retain(|k| !matches_deleted_leaf_prop(k));

        let was_explicit_prop: std::collections::HashSet<String> = self
            .explicit_prop_dirty_paths
            .iter()
            .filter(|k| matches_deleted_leaf_prop(k))
            .cloned()
            .collect();
        self.explicit_prop_dirty_paths
            .retain(|k| !matches_deleted_leaf_prop(k));

        let dropped_dirty_props: Vec<(String, bool)> = self
            .dirty_paths
            .iter()
            .filter_map(|k| {
                if !matches_deleted_leaf_prop(k) {
                    return None;
                }
                let rest = k
                    .strip_prefix(section_path)
                    .and_then(|s| s.strip_prefix('.'))?;
                let sibling_whole_entry_recorded = self
                    .dirty_map_entries
                    .contains(&format!("{section_path}\0{rest}"));
                let sibling_exists = pre_delete_keys.iter().any(|sk| sk == rest);
                if was_explicit_prop.contains(k)
                    || (!sibling_whole_entry_recorded && !sibling_exists)
                {
                    Some((k.clone(), !sibling_whole_entry_recorded))
                } else {
                    None
                }
            })
            .collect();
        if !dropped_dirty_props.is_empty() {
            for (old_k, remove_old) in dropped_dirty_props {
                if remove_old {
                    self.dirty_paths.remove(&old_k);
                }
            }
            self.dirty_paths
                .insert(format!("{section_path}.{deleted_key}"));
        }
        self.tracked_prop_identities.retain(|_, bound| {
            let bound_enc = &bound.encoded_key;
            bound_enc != &enc_deleted
                && !bound_enc.starts_with(&enc_deleted_dot)
                && !bound_enc.starts_with(&enc_deleted_bracket)
        });
    }

    /// Encode a non-secret dirty path (section or sub-table prefix, or whole entry)
    /// into the canonical snapshot key namespace used by external secret refs.
    fn encode_dirty_scope_prefix(&self, raw_path: &str) -> Option<String> {
        if let Some(enc) = self.encode_prop_snapshot_key(None, raw_path) {
            return Some(enc);
        }
        let normalized = raw_path.replace('-', "_");
        if Self::map_key_sections()
            .iter()
            .any(|s| s.path == raw_path || s.path == normalized)
        {
            return Some(normalized);
        }
        if let Some(section) = find_natural_key_section_for_path(raw_path) {
            let nk_values = self.get_map_keys(section.section_path)?;
            let rest = &raw_path[section.section_path.len() + 1..];
            if !self.explicit_prop_dirty_paths.contains(raw_path)
                && nk_values.iter().any(|k| k == rest)
            {
                return Some(crate::helpers::join_snapshot_path(
                    section.section_path,
                    &crate::helpers::encode_snapshot_segment(rest),
                ));
            }
            return match crate::helpers::route_vec_path(
                raw_path,
                "",
                section.section_path,
                "",
                nk_values.iter().enumerate().map(|(i, k)| (i, k.as_str())),
            ) {
                crate::helpers::VecRoute::Hit { index, inner_name } => {
                    Some(crate::helpers::join_snapshot_path(
                        &crate::helpers::join_snapshot_path(
                            section.section_path,
                            &crate::helpers::encode_snapshot_segment(&nk_values[index]),
                        ),
                        &inner_name.replace('-', "_"),
                    ))
                }
                _ => None,
            };
        }
        if let Some(section) = find_map_key_section_for_path(raw_path) {
            let outer_keys = self.get_map_keys(section.path)?;
            let rest = &raw_path[section.path.len() + 1..];
            if !self.explicit_prop_dirty_paths.contains(raw_path)
                && outer_keys.iter().any(|k| k == rest)
            {
                return Some(crate::helpers::join_snapshot_path(
                    section.path,
                    &crate::helpers::encode_snapshot_segment(rest),
                ));
            }
            let (matched_key, after_first) = crate::helpers::route_hashmap_path(
                raw_path,
                "",
                section.path,
                "",
                outer_keys.iter().map(String::as_str),
            )?;
            let enc_first = crate::helpers::join_snapshot_path(
                section.path,
                &crate::helpers::encode_snapshot_segment(matched_key),
            );
            let nested_map_path = format!("{}.{matched_key}", section.path);
            if let Some(inner_keys) = self.get_map_keys(&nested_map_path) {
                if !self.explicit_prop_dirty_paths.contains(raw_path)
                    && inner_keys.iter().any(|ik| ik == &after_first)
                {
                    return Some(crate::helpers::join_snapshot_path(
                        &enc_first,
                        &crate::helpers::encode_snapshot_segment(&after_first),
                    ));
                }
                let (inner_k, inner_tail) = crate::helpers::route_hashmap_path(
                    &after_first,
                    "",
                    "",
                    "",
                    inner_keys.iter().map(String::as_str),
                )?;
                return Some(crate::helpers::join_snapshot_path(
                    &crate::helpers::join_snapshot_path(
                        &enc_first,
                        &crate::helpers::encode_snapshot_segment(inner_k),
                    ),
                    &inner_tail.replace('-', "_"),
                ));
            }
            return Some(crate::helpers::join_snapshot_path(
                &enc_first,
                &after_first.replace('-', "_"),
            ));
        }
        Some(normalized)
    }

    fn collect_dirty_scope_prefixes(
        &self,
        whole_entries: &[(String, String)],
        dirty_prop_paths: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let mut prefixes = Vec::new();
        for (section, key) in whole_entries {
            prefixes.push(crate::helpers::join_snapshot_path(
                &encode_section_snapshot_prefix(section),
                &crate::helpers::encode_snapshot_segment(key),
            ));
        }
        for path in dirty_prop_paths {
            if !Self::prop_is_secret(path)
                && let Some(pfx) = self.encode_dirty_scope_prefix(path)
            {
                prefixes.push(pfx);
            }
        }
        prefixes
    }

    /// Prune active external-reference snapshots under natural-key `List`
    /// sections that are explicitly in scope for this save and now empty
    /// (`keys.is_empty()`), while leaving non-empty sections intact so
    /// unregistered natural-key mutations still fail closed.
    fn pruned_snapshots_for_restore(
        &self,
        scope_prefixes: &[String],
        full_create_fallback: bool,
    ) -> std::collections::HashMap<String, String> {
        let mut snaps = self.onepassword_reference_snapshots.to_map();
        if snaps.is_empty() {
            return snaps;
        }
        for section in Self::map_key_sections() {
            if section.kind != crate::traits::MapKeyKind::List || section.natural_key.is_none() {
                continue;
            }
            if !(full_create_fallback
                || snapshot_key_covered_by_prefixes(section.path, scope_prefixes))
            {
                continue;
            }
            if self
                .get_map_keys(section.path)
                .is_some_and(|keys| keys.is_empty())
            {
                let sec_dot = format!("{}.", section.path);
                let sec_bracket = format!("{}[", section.path);
                snaps.retain(|k, _| {
                    k != section.path && !k.starts_with(&sec_dot) && !k.starts_with(&sec_bracket)
                });
            }
        }
        snaps
    }

    /// Prepare live secret edits without mutating the running config. Resolve all
    /// references structurally in %-encoded snapshot-key space before any write;
    /// publish this candidate only after success.
    async fn prepare_dirty_secret_edits(
        &self,
        config_path: &Path,
        whole_entries: &[(String, String)],
        dirty_prop_paths: &std::collections::HashSet<String>,
        full_create_fallback: bool,
    ) -> Result<(Self, std::collections::HashSet<String>, Vec<String>)> {
        let mut masked_self = self.clone();
        let dirty_enc_keys = self.dirty_encoded_prop_keys(dirty_prop_paths);
        if !self.pre_override_snapshots.is_empty() {
            let non_dirty_snapshots: std::collections::HashMap<String, String> = self
                .pre_override_snapshots
                .iter()
                .filter(|(path, _)| {
                    !self.is_env_path_dirty(path, dirty_prop_paths, &dirty_enc_keys)
                })
                .collect();
            if !non_dirty_snapshots.is_empty() {
                crate::env_overrides::mask_env_overrides_for_save(
                    &mut masked_self,
                    &non_dirty_snapshots,
                )?;
            }
        }

        let mut live_refs = std::collections::HashMap::new();
        masked_self.collect_external_secret_refs(None, &mut live_refs)?;

        let scope_prefixes = self.collect_dirty_scope_prefixes(whole_entries, dirty_prop_paths);
        let explicit_dirty_enc_leaves: std::collections::HashSet<String> = dirty_prop_paths
            .iter()
            .filter(|path| Self::prop_is_secret(path))
            .filter_map(|path| self.encode_prop_snapshot_key(None, path))
            .collect();

        let env_isolated_enc_prefixes: Vec<String> = self
            .pre_override_snapshots
            .keys()
            .chain(self.env_overridden_paths.iter())
            .filter(|path| !self.is_env_path_dirty(path, dirty_prop_paths, &dirty_enc_keys))
            .filter_map(|path| {
                self.tracked_prop_identities
                    .get(path.as_str())
                    .map(|id| id.encoded_key.clone())
                    .or_else(|| self.encode_prop_snapshot_key(None, &path))
            })
            .collect();

        let mut refs_to_resolve: Vec<(String, String)> = live_refs
            .into_iter()
            .filter(|(enc_k, _)| {
                !snapshot_key_covered_by_prefixes(enc_k, &env_isolated_enc_prefixes)
                    && (full_create_fallback
                        || explicit_dirty_enc_leaves.contains(enc_k)
                        || explicit_dirty_enc_leaves
                            .iter()
                            .any(|arr| is_indexed_key_for_array_path(enc_k, arr))
                        || snapshot_key_covered_by_prefixes(enc_k, &scope_prefixes))
            })
            .collect();
        refs_to_resolve.sort_by(|a, b| a.0.cmp(&b.0));

        let mut candidate = self.clone();
        candidate.onepassword_reference_snapshots = ExternalRefSnapshots::from_map(
            self.pruned_snapshots_for_restore(&scope_prefixes, full_create_fallback),
        );
        let dir = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::secrets::SecretStore::new(dir, self.secrets.encrypt);

        tokio::task::spawn_blocking(move || {
            let mut resolved_plaintexts = std::collections::HashMap::new();
            let mut resolved_refs = std::collections::HashMap::new();
            for (enc_k, reference) in refs_to_resolve {
                let plaintext = store
                    .decrypt(&reference)
                    .with_context(|| format!("Failed to resolve external secret at {enc_k}"))?;
                resolved_plaintexts.insert(enc_k.clone(), plaintext);
                resolved_refs.insert(enc_k, reference);
            }
            if !resolved_plaintexts.is_empty() {
                let empty_skip = std::collections::HashSet::new();
                candidate.restore_external_secret_refs(None, &resolved_plaintexts, &empty_skip)?;
            }
            for enc_leaf in &explicit_dirty_enc_leaves {
                candidate
                    .onepassword_reference_snapshots
                    .retain(|k, _| k != enc_leaf && !is_indexed_key_for_array_path(k, enc_leaf));
            }
            let mut skip_restore_enc_keys = explicit_dirty_enc_leaves;
            for (enc_k, reference) in resolved_refs {
                candidate
                    .onepassword_reference_snapshots
                    .insert(enc_k.clone(), reference);
                skip_restore_enc_keys.insert(enc_k);
            }
            Ok((candidate, skip_restore_enc_keys, scope_prefixes))
        })
        .await
        .context("Config secret edit resolution task failed")?
    }

    /// Incremental save: only the paths in `self.dirty_paths` are written
    /// against the existing on-disk file. Non-dirty entries (including
    /// secret ciphertext) are left untouched; dirty paths whose value
    /// equals the schema default are removed from the doc instead of
    /// written. Falls back to a full `save()` when the file doesn't
    /// exist yet. Clears the dirty set on success.
    pub async fn save_dirty(&mut self) -> Result<()> {
        if self.dirty_paths.is_empty() {
            return Ok(());
        }

        let config_path = self.resolve_config_path_for_save().await?;
        if !config_path.exists() {
            let whole_entries = self.effective_whole_entry_dirty_entries(None);
            let dirty_prop_paths = self.collect_dirty_prop_paths(&whole_entries);
            self.verify_tracked_prop_identities(Some(&dirty_prop_paths))
                .map_err(anyhow::Error::msg)?;
            let (mut candidate, _skip_enc_keys, _scope_prefixes) = self
                .prepare_dirty_secret_edits(&config_path, &whole_entries, &dirty_prop_paths, true)
                .await?;
            return match self.save_impl(false, false).await {
                Ok((written, structural_refs)) => {
                    candidate
                        .onepassword_reference_snapshots
                        .retain(|k, _| structural_refs.contains_key(k));
                    // This value just created the file; `written` is the
                    // path the full save actually wrote.
                    let dirty_enc_keys = self.dirty_encoded_prop_keys(&dirty_prop_paths);
                    candidate.pre_override_snapshots.retain(|path, _| {
                        !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys)
                    });
                    candidate.env_overridden_paths.retain(|path| {
                        !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys)
                    });
                    candidate.loaded_from = Some(written);
                    candidate.clear_dirty();
                    *self = candidate;
                    Ok(())
                }
                Err(e) => Err(e),
            };
        }

        let existing = fs::read_to_string(&config_path).await.with_context(|| {
            format!(
                "Failed to read existing config for incremental save: {}",
                config_path.display()
            )
        })?;
        let mut doc: toml_edit::DocumentMut = existing
            .parse()
            .context("Failed to parse existing config for incremental save")?;
        let on_disk_table: Option<toml::Table> = toml::from_str::<toml::Table>(&existing).ok();
        if let Some(table) = on_disk_table.as_ref() {
            crate::migration::reject_retired_sop_config(table)?;
        }
        let whole_entries = self.effective_whole_entry_dirty_entries(on_disk_table.as_ref());
        let dirty_prop_paths = self.collect_dirty_prop_paths(&whole_entries);
        self.verify_tracked_prop_identities(Some(&dirty_prop_paths))
            .map_err(anyhow::Error::msg)?;

        let (mut live_candidate, skip_restore_enc_keys, scope_prefixes) = self
            .prepare_dirty_secret_edits(&config_path, &whole_entries, &dirty_prop_paths, false)
            .await?;
        let restorable_snapshots = self.pruned_snapshots_for_restore(&scope_prefixes, false);
        let mut config_to_save = self.clone();
        let zeroclaw_dir = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::secrets::SecretStore::new(zeroclaw_dir, self.secrets.encrypt);
        let dirty_enc_keys = self.dirty_encoded_prop_keys(&dirty_prop_paths);

        if !self.pre_override_snapshots.is_empty() {
            let non_dirty_snapshots: std::collections::HashMap<String, String> = self
                .pre_override_snapshots
                .iter()
                .filter(|(path, _)| {
                    !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys)
                })
                .collect();
            if !non_dirty_snapshots.is_empty() {
                crate::env_overrides::mask_env_overrides_for_save(
                    &mut config_to_save,
                    &non_dirty_snapshots,
                )?;
            }
        }
        restore_onepassword_references_for_save(
            &mut config_to_save,
            &restorable_snapshots,
            &skip_restore_enc_keys,
        )?;
        let mut structural_refs = std::collections::HashMap::new();
        config_to_save.collect_external_secret_refs(None, &mut structural_refs)?;
        config_to_save.encrypt_secrets(&store)?;

        let full_table: toml::Table = toml::Value::try_from(&config_to_save)
            .context("Failed to serialize config to TOML value")?
            .try_into()
            .context("Serialized config is not a TOML table")?;
        let default_table: toml::Table = toml::Value::try_from(Config::default())
            .ok()
            .and_then(|v| v.try_into().ok())
            .unwrap_or_default();

        for (section_path, entry_key) in &whole_entries {
            apply_dirty_whole_entry(
                doc.as_table_mut(),
                section_path,
                entry_key,
                &full_table,
                &default_table,
            );
        }
        for path in &dirty_prop_paths {
            apply_dirty_path(
                doc.as_table_mut(),
                path,
                &full_table,
                &default_table,
                self.explicit_prop_dirty_paths.contains(path),
            )?;
        }

        // Stamp the current schema version. An incremental save writes
        // current-schema-shaped sections (e.g. the dashboard saving a single
        // `agents.<name>.model_provider`) but `schema_version` is never a
        // dirty path, so without this it keeps whatever an older binary first
        // wrote on disk. The resulting body-newer-than-label file then crashes
        // older binaries with an opaque `missing field ...` serde error.
        // `insert` updates the existing key in place (preserving its
        // position at the top of the file) or appends it when absent.
        doc.as_table_mut().insert(
            "schema_version",
            toml_edit::value(i64::from(crate::migration::CURRENT_SCHEMA_VERSION)),
        );

        let toml_str = ensure_blank_line_before_sections(&doc.to_string());

        write_config_atomically(&config_path, &toml_str).await?;
        if !scope_prefixes.is_empty() {
            live_candidate
                .onepassword_reference_snapshots
                .retain(|k, _| {
                    !snapshot_key_covered_by_prefixes(k, &scope_prefixes)
                        || structural_refs.contains_key(k)
                });
        }
        live_candidate
            .pre_override_snapshots
            .retain(|path, _| !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys));
        live_candidate
            .env_overridden_paths
            .retain(|path| !self.is_env_path_dirty(path, &dirty_prop_paths, &dirty_enc_keys));
        live_candidate.clear_dirty();
        *self = live_candidate;
        Ok(())
    }
}

fn encode_section_snapshot_prefix(section_path: &str) -> String {
    if Config::map_key_sections()
        .iter()
        .any(|s| s.path == section_path)
    {
        return section_path.to_string();
    }
    if let Some(section) = find_map_key_section_for_path(section_path) {
        let outer_key = &section_path[section.path.len() + 1..];
        return format!(
            "{}.{}",
            section.path,
            crate::helpers::encode_snapshot_segment(outer_key)
        );
    }
    section_path.to_string()
}

fn is_indexed_key_for_array_path(key: &str, array_path: &str) -> bool {
    key.strip_prefix(array_path)
        .and_then(|rest| rest.strip_prefix('['))
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|idx| idx.parse::<usize>().is_ok())
}

fn lookup_inner_value_in_table<'a>(
    mut current_table: &'a toml::Table,
    dotted: &str,
) -> Option<&'a toml::Value> {
    let mut rest = dotted;
    loop {
        if let Some(val) = current_table.get(rest) {
            return Some(val);
        }
        let snake_rest = rest.replace('-', "_");
        if let Some(val) = current_table.get(&snake_rest) {
            return Some(val);
        }
        let (seg, next_rest) = rest.split_once('.')?;
        let next_val = current_table
            .get(seg)
            .or_else(|| current_table.get(&seg.replace('-', "_")))?;
        current_table = next_val.as_table()?;
        rest = next_rest;
    }
}

pub(crate) fn lookup_prop_value_in_table<'a>(
    full_table: &'a toml::Table,
    dotted: &str,
) -> Option<&'a toml::Value> {
    if let Some(section) = find_natural_key_section_for_path(dotted) {
        let section_segs: Vec<&str> = section.section_path.split('.').collect();
        let mem_array = lookup_path_in_table(full_table, &section_segs)?.as_array()?;
        let natural_keys = mem_array.iter().enumerate().filter_map(|(idx, item)| {
            Some((
                idx,
                item.as_table()?.get(section.natural_key_field)?.as_str()?,
            ))
        });
        return match crate::helpers::route_vec_path(
            dotted,
            "",
            section.section_path,
            "",
            natural_keys,
        ) {
            crate::helpers::VecRoute::Hit { index, inner_name } => {
                let entry_table = mem_array.get(index)?.as_table()?;
                lookup_inner_value_in_table(entry_table, &inner_name)
            }
            _ => None,
        };
    }

    if let Some(section) = find_map_key_section_for_path(dotted) {
        let section_segs: Vec<&str> = section.path.split('.').collect();
        let mem_table = lookup_path_in_table(full_table, &section_segs)?.as_table()?;
        let (key, inner) = crate::helpers::route_hashmap_path(
            dotted,
            "",
            section.path,
            "",
            mem_table.keys().map(String::as_str),
        )?;
        let key_table = mem_table.get(key)?.as_table()?;
        return lookup_inner_value_in_table(key_table, &inner).or_else(|| {
            let (inner_key, leaf_tail) = crate::helpers::route_hashmap_path(
                &inner,
                "",
                "",
                "",
                key_table.keys().map(String::as_str),
            )?;
            let nested_table = key_table.get(inner_key)?.as_table()?;
            lookup_inner_value_in_table(nested_table, &leaf_tail)
        });
    }

    lookup_inner_value_in_table(full_table, dotted)
}

pub(crate) fn collect_onepassword_reference_snapshots(
    config: &Config,
) -> Result<std::collections::HashMap<String, String>> {
    let mut snapshots = std::collections::HashMap::new();
    config.collect_external_secret_refs(None, &mut snapshots)?;
    Ok(snapshots)
}

fn snapshot_key_covered_by_prefixes(key: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|pfx| {
        key == pfx
            || key
                .strip_prefix(pfx.as_str())
                .is_some_and(|tail| tail.starts_with('.') || tail.starts_with('['))
    })
}

fn restore_onepassword_references_for_save(
    config_to_save: &mut Config,
    snapshots: &std::collections::HashMap<String, String>,
    skip_encoded_dirty: &std::collections::HashSet<String>,
) -> Result<()> {
    if snapshots.is_empty() {
        return Ok(());
    }
    config_to_save.restore_external_secret_refs(None, snapshots, skip_encoded_dirty)
}

/// Atomic write shared by `save()` and `save_dirty()`.
async fn write_config_atomically(config_path: &Path, toml_str: &str) -> Result<()> {
    let parent_dir = config_path
        .parent()
        .context("Config path must have a parent directory")?;

    fs::create_dir_all(parent_dir).await.with_context(|| {
        format!(
            "Failed to create config directory: {}",
            parent_dir.display()
        )
    })?;

    let file_name = config_path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("config.toml");
    let temp_path = parent_dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
    let backup_path = parent_dir.join(format!("{file_name}.bak"));

    let mut temp_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .await
        .with_context(|| {
            format!(
                "Failed to create temporary config file: {}",
                temp_path.display()
            )
        })?;
    temp_file
        .write_all(toml_str.as_bytes())
        .await
        .context("Failed to write temporary config contents")?;
    temp_file
        .sync_all()
        .await
        .context("Failed to fsync temporary config file")?;
    drop(temp_file);

    let had_existing_config = config_path.exists();
    if had_existing_config {
        fs::copy(config_path, &backup_path).await.with_context(|| {
            format!(
                "Failed to create config backup before atomic replace: {}",
                backup_path.display()
            )
        })?;
    }

    if let Err(e) = fs::rename(&temp_path, config_path).await {
        let _ = fs::remove_file(&temp_path).await;
        if had_existing_config && backup_path.exists() {
            fs::copy(&backup_path, config_path)
                .await
                .context("Failed to restore config backup")?;
        }
        anyhow::bail!("Failed to atomically replace config file: {e}");
    }

    #[cfg(unix)]
    {
        use std::{fs::Permissions, os::unix::fs::PermissionsExt};
        if let Err(err) = fs::set_permissions(config_path, Permissions::from_mode(0o600)).await {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(
                    crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                    ::zeroclaw_log::Action::Note
                )
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                &format!(
                    "Failed to harden config permissions to 0600 at {}: {}",
                    config_path.display().to_string(),
                    err
                )
            );
        }
    }

    sync_directory(parent_dir).await?;

    if had_existing_config {
        let _ = fs::remove_file(&backup_path).await;
    }

    Ok(())
}

fn apply_dirty_whole_entry(
    root: &mut toml_edit::Table,
    section_path: &str,
    entry_key: &str,
    full_table: &toml::Table,
    default_table: &toml::Table,
) {
    if let Some(nk_field) = Config::map_key_sections().into_iter().find_map(|s| {
        (s.kind == crate::traits::MapKeyKind::List && s.path == section_path)
            .then_some(s.natural_key)
            .flatten()
    }) {
        let section = NaturalKeySection {
            section_path: Config::map_key_sections()
                .into_iter()
                .find(|s| s.path == section_path)
                .map(|s| s.path)
                .unwrap_or(""),
            natural_key_field: nk_field,
        };
        if !section.section_path.is_empty() {
            reconcile_natural_key_whole_entry(root, &section, entry_key, full_table);
        }
        return;
    }

    let mut segs: Vec<&str> = section_path.split('.').collect();
    segs.push(entry_key);
    write_or_delete_leaf(root, full_table, default_table, &segs);
}

/// Write the in-memory value at `dotted` into the doc, or delete the leaf
/// when the value is absent or equals the schema default. Segments are
/// kebab→snake-translated; alias keys never carry hyphens (alias rule).
fn apply_dirty_path(
    root: &mut toml_edit::Table,
    dotted: &str,
    full_table: &toml::Table,
    default_table: &toml::Table,
    is_explicit_prop_edit: bool,
) -> Result<()> {
    if dotted.is_empty() {
        return Ok(());
    }

    // Natural-key `Vec<T>` sections (`#[natural_key = "<f>"]`, currently
    // `mcp.servers`) serialize as `[[mcp.servers]]` — an
    // `Item::ArrayOfTables`, not a Table. The generic walker below
    // assumes every intermediate node is a Table (`as_table()` /
    // `as_table_mut()`), so without this branch a dirty path like
    // `mcp.servers.fs.command` would resolve to `None` in `full_table`,
    // get misclassified as `should_delete`, then bail at the
    // `ArrayOfTables` segment in `delete_path_in_doc` too. Net effect:
    // the per-field MCP editor's edits go nowhere on disk. See the
    // regression test `save_dirty_persists_mcp_server_field_via_natural_key`.
    if let Some(section) = find_natural_key_section_for_path(dotted) {
        return apply_dirty_natural_key_path(
            root,
            &section,
            dotted,
            full_table,
            default_table,
            is_explicit_prop_edit,
        );
    }

    // `HashMap<String, T>` sections whose key is a `#[resource_key]`
    // (drawn from another domain — a model id, tool name, …) may
    // themselves contain dots, e.g. `gpt-4.1` in
    // `cost.rates.providers.models.openai.gpt-4.1.input_per_mtok`. The
    // blind `raw.split('.')` above and the generic walker below both
    // assume one segment == one dot-delimited token, so a dotted key
    // fragments into bogus segments (`gpt-4`, `1`), the lookup below
    // finds nothing, and the write silently turns into a no-op delete.
    // Longest-match the key against the section's live keys instead —
    // same precedent as the natural-key branch above.
    if let Some(section) = find_map_key_section_for_path(dotted) {
        return apply_dirty_map_key_path(
            root,
            &section,
            dotted,
            full_table,
            default_table,
            is_explicit_prop_edit,
        );
    }

    // Resolve each segment against the in-memory table: struct fields
    // serialize as snake_case (so `input-per-mtok` → `input_per_mtok`), but
    // HashMap keys are preserved verbatim and may legitimately carry hyphens
    // (`claude-opus-4-7`, `tts-1-hd`) or dots (`X-My.Header`).
    let segments = resolve_dirty_table_segments(Some(full_table), Some(root), dotted);
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    write_or_delete_leaf(root, full_table, default_table, &segs);
    Ok(())
}

/// Resolved metadata for a `#[natural_key]` Vec section that matches a
/// given dirty path. `section_path` is the dotted section (e.g.
/// `"mcp.servers"`); `natural_key_field` is the snake_case field name
/// on the inner type that holds each element's alias (e.g. `"name"`).
struct NaturalKeySection {
    section_path: &'static str,
    natural_key_field: &'static str,
}

/// Return the longest natural-key `Kind::List` section whose path is a
/// strict prefix of `dotted` (i.e. `dotted` starts with `<path>.`).
/// Strict-prefix wins so a nested natural-key section would always
/// outscore its parent; today there's only `mcp.servers`, so the lookup
/// is effectively a single-entry match, but the longest-match rule
/// keeps the writer correct when a future `#[natural_key]` Vec lands
/// inside another section.
///
/// Returns the `<section_path>` (e.g. `"mcp.servers"`) and the
/// `<natural_key_field>` (e.g. `"name"`) the writer needs to locate
/// the matching `[[<section_path>]]` element on disk.
///
/// Section paths are exact-matched against the leading segments of
/// `dotted`; the function does **not** apply the kebab→snake fallback
/// that the inner segment resolver (`resolve_dirty_segments`) uses.
/// That is deliberate and mirrors the in-memory routing macro's
/// `section_path == expected` check in
/// `crates/zeroclaw-macros/src/lib.rs` (the `get_map_keys` /
/// `route_vec_path` arms) — making either side dash-tolerant in
/// isolation would let the dispatcher and the writer disagree on
/// which call participates in this branch, which is exactly the
/// memory-vs-disk divergence this PR exists to eliminate. If a
/// future natural-key section needs an underscore in its path AND
/// must be addressable via a kebab wire path, change **both** sides
/// together (the macro arm and this match).
///
/// Plain `Kind::List` Vec sections without `#[natural_key]` (their
/// `natural_key` is `None`) are intentionally excluded — those use the
/// legacy whole-array `ObjectArray` save path and don't need the
/// array-of-tables walker.
fn find_natural_key_section_for_path(dotted: &str) -> Option<NaturalKeySection> {
    use crate::traits::MapKeyKind;

    let mut best: Option<NaturalKeySection> = None;
    for section in Config::map_key_sections().iter() {
        if section.kind != MapKeyKind::List {
            continue;
        }
        let Some(natural_key_field) = section.natural_key else {
            continue;
        };
        // strip_prefix avoids the allocation `format!("{}.", ...)`
        // would do per section per dirty path, and makes the "must
        // have *something* after the dot" guard a natural fallout of
        // the second `strip_prefix('.')` rather than a length check.
        let Some(after) = dotted
            .strip_prefix(section.path)
            .and_then(|s| s.strip_prefix('.'))
        else {
            continue;
        };
        // Empty alias segment ("mcp.servers." with nothing after) can't
        // be addressed by name; skip to avoid matching every otherwise-
        // unmatched path via the empty prefix. Same guard the in-memory
        // `route_vec_path` applies to empty natural keys.
        if after.is_empty() {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|b| section.path.len() > b.section_path.len());
        if better {
            best = Some(NaturalKeySection {
                section_path: section.path,
                natural_key_field,
            });
        }
    }
    best
}

/// Apply a dirty path of the form
/// `<section_path>.<alias>[.<inner_suffix>...]` against an
/// `[[<section_path>]]` array of tables.
///
/// Three shapes show up in practice:
///
///   1. `<section>.<alias>.<inner>` — per-field edit (the dashboard /
///      TUI editor). Locate the in-memory element whose natural-key
///      field equals `<alias>` and reconcile its `<inner>` sub-path
///      against the doc's matching `[[<section>]]` table.
///
///   2. `<section>.<alias>` with the element still in memory — the
///      whole-element write the rename path emits for the *new* alias.
///      Reseed the matching doc entry from the in-memory serialization
///      so a rename `fs` → `filesystem` updates the entire
///      `[[mcp.servers]]` table (in particular its `name` field).
///
///   3. `<section>.<alias>` with the element gone from memory — the
///      delete path the rename emits for the *old* alias, and what
///      `delete_map_key` plus an explicit `mark_dirty` would produce.
///      Drop the matching doc entry.
///
/// All three are handled by walking the doc through the section's
/// non-array parents, locating the `ArrayOfTables` at the section's
/// last path segment, then either editing or removing the entry whose
/// natural-key field matches `<alias>`. The `<inner>` recursion reuses
/// the existing Table-shaped `set_path_in_doc` / `delete_path_in_doc`
/// helpers against the located entry's table.
///
/// **Order-independence invariant.** `Config::dirty_paths` is a
/// `HashSet<String>`, so the three shapes above can apply in any
/// interleaving within a single `save_dirty` batch — in particular,
/// a rename emits both the old and new aliases dirty without an
/// ordering guarantee, and a create-then-edit sequence may walk the
/// per-field path before the whole-element write. Convergence under
/// every ordering is load-bearing here and works only because every
/// branch derives its write from the current in-memory state and the
/// case-1 delete fires only on mem-absence (not on default
/// equivalence). Future edits — e.g. an early-exit that assumes
/// whole-element writes run before per-field ones, or a "skip if
/// already written" optimization keyed on the doc shape — would
/// break this. Preserve commutativity across the three shapes for
/// any single (section, alias) tuple.
fn lookup_array_of_tables_keys(
    root: &toml_edit::Table,
    parent_segs: &[&str],
    array_key: &str,
    natural_key_field: &str,
) -> Vec<String> {
    let mut cursor: &toml_edit::Table = root;
    for seg in parent_segs {
        let Some(next) = cursor.get(seg).and_then(toml_edit::Item::as_table) else {
            return Vec::new();
        };
        cursor = next;
    }
    let Some(aot) = cursor
        .get(array_key)
        .and_then(toml_edit::Item::as_array_of_tables)
    else {
        return Vec::new();
    };
    aot.iter()
        .filter_map(|t| t.get(natural_key_field)?.as_str().map(str::to_string))
        .collect()
}

fn reconcile_natural_key_whole_entry(
    root: &mut toml_edit::Table,
    section: &NaturalKeySection,
    alias: &str,
    full_table: &toml::Table,
) {
    if alias.is_empty() {
        return;
    }
    let section_segs: Vec<&str> = section.section_path.split('.').collect();
    let Some((array_key, parent_segs)) = section_segs.split_last() else {
        return;
    };
    let mem_array = lookup_path_in_table(full_table, &section_segs).and_then(|v| v.as_array());
    let mem_entry = mem_array.and_then(|arr| {
        arr.iter().find_map(|item| {
            let table = item.as_table()?;
            let alias_value = table.get(section.natural_key_field)?.as_str()?;
            (alias_value == alias).then_some(table)
        })
    });
    match mem_entry {
        Some(entry) => {
            let mut value = toml::Value::Table(entry.clone());
            prune_empty_leaves(&mut value);
            let Some(entry_table) = ensure_array_of_tables_entry(
                root,
                parent_segs,
                array_key,
                section.natural_key_field,
                alias,
            ) else {
                return;
            };
            if let toml::Value::Table(t) = value {
                crate::migration::sync_table(entry_table, &t);
            }
        }
        None => {
            delete_array_of_tables_entry(
                root,
                parent_segs,
                array_key,
                section.natural_key_field,
                alias,
            );
        }
    }
}

fn apply_dirty_natural_key_path(
    root: &mut toml_edit::Table,
    section: &NaturalKeySection,
    dotted: &str,
    full_table: &toml::Table,
    _default_table: &toml::Table,
    is_explicit_prop_edit: bool,
) -> Result<()> {
    let rest = match dotted
        .strip_prefix(section.section_path)
        .and_then(|s| s.strip_prefix('.'))
    {
        Some(r) if !r.is_empty() => r,
        _ => return Ok(()),
    };

    let section_segs: Vec<&str> = section.section_path.split('.').collect();
    let Some((array_key, parent_segs)) = section_segs.split_last() else {
        return Ok(());
    };
    let mem_array = lookup_path_in_table(full_table, &section_segs).and_then(|v| v.as_array());
    let mem_keys: Vec<String> = mem_array
        .into_iter()
        .flat_map(|arr| arr.iter())
        .filter_map(|item| {
            item.as_table()?
                .get(section.natural_key_field)?
                .as_str()
                .map(str::to_string)
        })
        .collect();
    let doc_keys =
        lookup_array_of_tables_keys(root, parent_segs, array_key, section.natural_key_field);

    if !is_explicit_prop_edit
        && (mem_keys.iter().any(|k| k == rest) || doc_keys.iter().any(|k| k == rest))
    {
        reconcile_natural_key_whole_entry(root, section, rest, full_table);
        return Ok(());
    }

    let (alias, inner) = match crate::helpers::route_vec_path(
        dotted,
        "",
        section.section_path,
        "",
        mem_keys.iter().enumerate().map(|(i, k)| (i, k.as_str())),
    ) {
        crate::helpers::VecRoute::Hit { index, inner_name } => {
            (mem_keys[index].clone(), inner_name)
        }
        crate::helpers::VecRoute::Ambiguous { key, count } => {
            anyhow::bail!(
                "save_dirty: natural key `{key}` is ambiguous in `{}` ({count} entries share it)",
                section.section_path
            );
        }
        crate::helpers::VecRoute::Miss => match crate::helpers::route_vec_path(
            dotted,
            "",
            section.section_path,
            "",
            doc_keys.iter().enumerate().map(|(i, k)| (i, k.as_str())),
        ) {
            crate::helpers::VecRoute::Hit { index, inner_name } => {
                (doc_keys[index].clone(), inner_name)
            }
            crate::helpers::VecRoute::Ambiguous { key, count } => {
                anyhow::bail!(
                    "save_dirty: natural key `{key}` is ambiguous in `{}` ({count} entries share it)",
                    section.section_path
                );
            }
            crate::helpers::VecRoute::Miss => return Ok(()),
        },
    };

    let mem_entry = mem_array.and_then(|arr| {
        arr.iter().find_map(|item| {
            let table = item.as_table()?;
            let alias_value = table.get(section.natural_key_field)?.as_str()?;
            (alias_value == alias).then_some(table)
        })
    });
    let inner_segments: Vec<String> = {
        let doc_entry = find_array_of_tables_entry_mut(
            root,
            parent_segs,
            array_key,
            section.natural_key_field,
            &alias,
        )
        .map(|t| t as &dyn toml_edit::TableLike);
        resolve_dirty_table_segments(mem_entry, doc_entry, &inner)
    };
    let inner_segs: Vec<&str> = inner_segments.iter().map(String::as_str).collect();
    let mem_inner_val = mem_entry.and_then(|t| lookup_path_in_table(t, &inner_segs));

    match mem_inner_val {
        Some(value) => {
            let Some(entry_table) = ensure_array_of_tables_entry(
                root,
                parent_segs,
                array_key,
                section.natural_key_field,
                &alias,
            ) else {
                return Ok(());
            };
            let mut pruned = value.clone();
            prune_empty_leaves(&mut pruned);
            set_path_in_doc(entry_table, &inner_segs, &pruned);
        }
        None => {
            let Some(entry_table) = find_array_of_tables_entry_mut(
                root,
                parent_segs,
                array_key,
                section.natural_key_field,
                &alias,
            ) else {
                return Ok(());
            };
            delete_path_in_doc(entry_table, &inner_segs);
        }
    }
    Ok(())
}

/// Return the longest `MapKeyKind::Map` section whose path is a strict
/// prefix of `dotted` (i.e. `dotted` starts with `<path>.`). Longest-match
/// mirrors `find_natural_key_section_for_path`: a nested `HashMap` field's
/// own section always outscores a shorter ancestor section.
fn find_map_key_section_for_path(dotted: &str) -> Option<crate::traits::MapKeySection> {
    use crate::traits::MapKeyKind;

    let mut best: Option<crate::traits::MapKeySection> = None;
    for section in Config::map_key_sections() {
        if section.kind != MapKeyKind::Map {
            continue;
        }
        let Some(after) = dotted
            .strip_prefix(section.path)
            .and_then(|s| s.strip_prefix('.'))
        else {
            continue;
        };
        if after.is_empty() {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|b| section.path.len() > b.path.len());
        if better {
            best = Some(section);
        }
    }
    best
}

/// Apply a dirty path of the form `<section_path>.<key>[.<inner_suffix>...]`
/// against a `HashMap<String, T>` section. `<key>` is the literal map key
/// and, for `#[resource_key]` sections (a model id, tool name, … rather
/// than an operator-chosen alias), may itself contain dots — e.g. `gpt-4.1`
/// in `cost.rates.providers.models.openai.gpt-4.1.input_per_mtok`.
///
/// Longest-match `<key>` against the union of keys live in `full_table`
/// (the in-memory state) and in the doc (`root`), so a key that only
/// survives on one side — the delete half of a rename, or a fresh
/// alias not yet flushed — still resolves. This mirrors
/// `apply_dirty_natural_key_path`'s two-sided lookup for `mcp.servers`.
///
/// A whole-entry write (no inner suffix — `<section_path>.<key>` addresses
/// the map value itself, e.g. the create-map-key path) is the case where
/// `<key>` consumes the entire remainder; that always wins over any
/// shorter prefix-plus-dot match into the same key space, so it's checked
/// first.
///
/// Once `<key>` is resolved, the rest of the path reuses the exact
/// mem-vs-default comparison and doc write/delete the generic Table-shaped
/// walker in `apply_dirty_path` uses — the dotted key is just folded into
/// one opaque segment instead of being split on '.' with everything else.
fn apply_dirty_map_key_path(
    root: &mut toml_edit::Table,
    section: &crate::traits::MapKeySection,
    dotted: &str,
    full_table: &toml::Table,
    default_table: &toml::Table,
    is_explicit_prop_edit: bool,
) -> Result<()> {
    let section_segs: Vec<&str> = section.path.split('.').collect();
    let remainder = &dotted[section.path.len() + 1..];

    let mem_table = lookup_path_in_table(full_table, &section_segs).and_then(|v| v.as_table());
    let doc_table = lookup_table_in_doc(root, &section_segs);

    let mem_keys: Vec<&str> = mem_table
        .into_iter()
        .flat_map(|t| t.keys().map(String::as_str))
        .collect();
    let doc_keys: Vec<&str> = doc_table
        .into_iter()
        .flat_map(|t| t.iter().map(|(k, _)| k))
        .collect();
    let has_exact_entry = mem_keys.contains(&remainder) || doc_keys.contains(&remainder);

    let key_match: Option<(&str, Option<String>)> = if !is_explicit_prop_edit && has_exact_entry {
        Some((remainder, None))
    } else {
        crate::helpers::route_hashmap_path(dotted, "", section.path, "", mem_keys.iter().copied())
            .or_else(|| {
                crate::helpers::route_hashmap_path(
                    dotted,
                    "",
                    section.path,
                    "",
                    doc_keys.iter().copied(),
                )
            })
            .map(|(key, inner)| (key, Some(inner)))
            .or_else(|| has_exact_entry.then_some((remainder, None)))
    };

    let Some((key, inner)) = key_match else {
        anyhow::bail!(
            "save_dirty: dirty path `{dotted}` addresses map-key section `{}` but its key \
             resolves in neither the in-memory config nor the on-disk file; refusing to \
             silently drop the write",
            section.path
        );
    };

    let mut segments: Vec<String> = section_segs.iter().map(|s| (*s).to_string()).collect();
    segments.push(key.to_string());

    if let Some(inner) = inner {
        let key_table = mem_table
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_table());
        let doc_key_table = doc_table
            .and_then(|t| t.get(key))
            .and_then(toml_edit::Item::as_table_like);
        let inner_segments = resolve_dirty_table_segments(key_table, doc_key_table, &inner);
        segments.extend(inner_segments);
    }

    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    write_or_delete_leaf(root, full_table, default_table, &segs);
    Ok(())
}

/// Shared tail of the generic and map-key dirty-path walkers: reconcile
/// the resolved `segs` against the doc — delete the leaf when the
/// in-memory value is absent or equals the schema default, otherwise
/// write the pruned in-memory value. One copy so a future fix to the
/// delete-vs-write rule cannot land in one walker only.
fn write_or_delete_leaf(
    root: &mut toml_edit::Table,
    full_table: &toml::Table,
    default_table: &toml::Table,
    segs: &[&str],
) {
    let mem_val = lookup_path_in_table(full_table, segs);
    let default_val = lookup_path_in_table(default_table, segs);

    let should_delete = match (mem_val, default_val) {
        (None, _) => true,
        (Some(m), Some(d)) if m == d => true,
        _ => false,
    };

    if should_delete {
        delete_path_in_doc(root, segs);
    } else if let Some(value) = mem_val {
        let mut pruned = value.clone();
        prune_empty_leaves(&mut pruned);
        set_path_in_doc(root, segs, &pruned);
    }
}

/// Emit a single `WARN` event when a natural-key writer (ensure /
/// find / delete) bails because the on-disk node has the wrong shape
/// — for example an operator hand-edited `mcp.servers = "foo"`, or
/// wrote `servers = [{ name = "fs", ... }]` (an inline array of
/// tables, which `toml_edit` parses as `Item::Value(Value::Array)`
/// rather than the `Item::ArrayOfTables` this writer produces).
///
/// Without this, `config/set` returns success, the in-memory mutation
/// and the dashboard/TUI both reflect the new value, and the on-disk
/// file silently stays stale — the exact symptom the underlying
/// natural-key persistence bug ( fix) exists to eliminate. The
/// `WARN` shape mirrors the 0600-permissions fallback below: same
/// `Action::Note` / `EventOutcome::Unknown` / module path, so log
/// scrapers that already understand the schema warnings will surface
/// these the same way.
///
/// `op` is `"ensure"`, `"find"`, or `"delete"` — which writer bailed.
/// `kind` is `"parent_segment"` or `"array_key"` — which node was
/// wrong-shaped. `which` is the specific seg/key name involved.
fn warn_natural_key_doc_kind_mismatch(
    parent_segs: &[&str],
    array_key: &str,
    alias: &str,
    kind: &str,
    which: &str,
    op: &str,
) {
    let section_path = if parent_segs.is_empty() {
        array_key.to_string()
    } else {
        format!("{}.{array_key}", parent_segs.join("."))
    };
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(
            crate::schema::CONFIG_SCHEMA_LOG_TARGET,
            ::zeroclaw_log::Action::Note
        )
        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
        .with_attrs(::serde_json::json!({
            "section_path": section_path,
            "alias": alias,
            "node_kind": kind,
            "node_name": which,
            "operation": op,
        })),
        "natural-key writer: refusing to clobber hand-edited config.toml node; \
         on-disk file may diverge from in-memory state"
    );
}

/// Locate or create the `[[<section_path>]]` entry whose natural-key
/// field equals `alias`, returning a mutable reference to its inner
/// `toml_edit::Table`.
///
/// Three shapes are reconciled here:
///
///   * Parent Tables along `parent_segs` are auto-created if missing
///     (mirrors `set_path_in_doc`).
///   * The `array_key` slot may not yet exist; insert an
///     `ArrayOfTables` if so.
///   * The array may not yet contain an entry whose natural-key field
///     matches `alias`; push a new table with `<natural_key_field> =
///     "<alias>"` seeded.
///
/// Returns `None` only when an existing doc node at `parent_segs` or
/// `array_key` has the wrong kind (e.g. an operator hand-edited
/// `mcp.servers = "foo"`, or wrote an inline `servers = [{ ... }]`
/// array-of-tables that parses as `Item::Value(Value::Array)` rather
/// than the `Item::ArrayOfTables` shape this writer expects); in that
/// case we refuse to clobber the hand-edit and let the save bail
/// rather than data-loss the user's file. A `WARN`-level event is
/// emitted on every wrong-kind bail so the divergence between memory
/// and disk is observable in logs — silent bails were what let the
/// underlying bug ship.
fn ensure_array_of_tables_entry<'a>(
    root: &'a mut toml_edit::Table,
    parent_segs: &[&str],
    array_key: &str,
    natural_key_field: &str,
    alias: &str,
) -> Option<&'a mut toml_edit::Table> {
    let mut cursor: &mut toml_edit::Table = root;
    for seg in parent_segs {
        if !cursor.contains_key(seg) {
            cursor.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let next = cursor.get_mut(seg).and_then(|i| i.as_table_mut());
        if next.is_none() {
            warn_natural_key_doc_kind_mismatch(
                parent_segs,
                array_key,
                alias,
                "parent_segment",
                seg,
                "ensure",
            );
            return None;
        }
        cursor = next?;
    }
    if !cursor.contains_key(array_key) {
        cursor.insert(
            array_key,
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    let aot = cursor
        .get_mut(array_key)
        .and_then(|i| i.as_array_of_tables_mut());
    let Some(aot) = aot else {
        warn_natural_key_doc_kind_mismatch(
            parent_segs,
            array_key,
            alias,
            "array_key",
            array_key,
            "ensure",
        );
        return None;
    };

    let pos = aot
        .iter()
        .position(|t| t.get(natural_key_field).and_then(|i| i.as_str()) == Some(alias));
    let idx = match pos {
        Some(i) => i,
        None => {
            let mut new_table = toml_edit::Table::new();
            new_table.insert(natural_key_field, toml_edit::value(alias.to_string()));
            aot.push(new_table);
            aot.len() - 1
        }
    };
    aot.get_mut(idx)
}

/// Read-only counterpart to [`ensure_array_of_tables_entry`] used by
/// the delete path: never creates a missing array or entry, just
/// locates one if present. Returning `None` on a miss lets the caller
/// skip the doc-mutation step entirely. Distinguishes "node doesn't
/// exist" (silent no-op — the legitimate fresh-alias case) from "node
/// exists but has the wrong kind" (a `WARN`-logged bail — the
/// hand-edited inline-array hazard).
fn find_array_of_tables_entry_mut<'a>(
    root: &'a mut toml_edit::Table,
    parent_segs: &[&str],
    array_key: &str,
    natural_key_field: &str,
    alias: &str,
) -> Option<&'a mut toml_edit::Table> {
    let mut cursor: &mut toml_edit::Table = root;
    for seg in parent_segs {
        let item = cursor.get_mut(seg)?;
        let Some(next) = item.as_table_mut() else {
            warn_natural_key_doc_kind_mismatch(
                parent_segs,
                array_key,
                alias,
                "parent_segment",
                seg,
                "find",
            );
            return None;
        };
        cursor = next;
    }
    let item = cursor.get_mut(array_key)?;
    let Some(aot) = item.as_array_of_tables_mut() else {
        warn_natural_key_doc_kind_mismatch(
            parent_segs,
            array_key,
            alias,
            "array_key",
            array_key,
            "find",
        );
        return None;
    };
    let pos = aot
        .iter()
        .position(|t| t.get(natural_key_field).and_then(|i| i.as_str()) == Some(alias))?;
    aot.get_mut(pos)
}

/// Remove the `[[<section_path>]]` entry whose natural-key field
/// equals `alias`. No-op if the array or entry isn't present. When the
/// removal empties the `ArrayOfTables`, drop the array slot too so the
/// on-disk file doesn't carry a vestigial `[[mcp.servers]]`-shaped
/// blank section header.
fn delete_array_of_tables_entry(
    root: &mut toml_edit::Table,
    parent_segs: &[&str],
    array_key: &str,
    natural_key_field: &str,
    alias: &str,
) {
    let mut cursor: &mut toml_edit::Table = root;
    for seg in parent_segs {
        let Some(item) = cursor.get_mut(seg) else {
            return;
        };
        match item.as_table_mut() {
            Some(next) => cursor = next,
            None => {
                warn_natural_key_doc_kind_mismatch(
                    parent_segs,
                    array_key,
                    alias,
                    "parent_segment",
                    seg,
                    "delete",
                );
                return;
            }
        }
    }
    let Some(item) = cursor.get_mut(array_key) else {
        return;
    };
    let aot = match item.as_array_of_tables_mut() {
        Some(a) => a,
        None => {
            warn_natural_key_doc_kind_mismatch(
                parent_segs,
                array_key,
                alias,
                "array_key",
                array_key,
                "delete",
            );
            return;
        }
    };
    let pos = aot
        .iter()
        .position(|t| t.get(natural_key_field).and_then(|i| i.as_str()) == Some(alias));
    if let Some(idx) = pos {
        aot.remove(idx);
    }
    if aot.is_empty() {
        cursor.remove(array_key);
    }
}

/// Drop empty arrays / tables / strings from a value before writing it
/// to the doc. HashMap entries serialize every default field (no
/// `skip_serializing_if` on individual `Vec<String>` fields), so without
/// this pass an `mcp_bundles.<alias>` write produces `servers = []`,
/// `exclude = []`, etc. The pruned form round-trips identically because
/// each dropped field's serde default IS the dropped value.
fn prune_empty_leaves(value: &mut toml::Value) {
    match value {
        toml::Value::Table(t) => {
            let keys: Vec<String> = t.keys().cloned().collect();
            for key in keys {
                if let Some(inner) = t.get_mut(&key) {
                    prune_empty_leaves(inner);
                }
                let drop = match t.get(&key) {
                    Some(toml::Value::Array(arr)) => arr.is_empty(),
                    Some(toml::Value::Table(inner)) => inner.is_empty(),
                    Some(toml::Value::String(s)) => s.is_empty(),
                    _ => false,
                };
                if drop {
                    t.remove(&key);
                }
            }
        }
        toml::Value::Array(arr) => {
            for item in arr.iter_mut() {
                prune_empty_leaves(item);
            }
        }
        _ => {}
    }
}

fn resolve_dirty_table_segments(
    mut mem_table: Option<&toml::Table>,
    mut doc_table: Option<&dyn toml_edit::TableLike>,
    dotted: &str,
) -> Vec<String> {
    if dotted.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut rest = dotted;
    loop {
        if let Some(t) = mem_table {
            if t.contains_key(rest) {
                out.push(rest.to_string());
                return out;
            }
            let snake_rest = rest.replace('-', "_");
            if t.contains_key(&snake_rest) {
                out.push(snake_rest);
                return out;
            }
        }
        if let Some(dt) = doc_table
            && dt.contains_key(rest)
        {
            out.push(rest.to_string());
            return out;
        }

        let Some((seg, next_rest)) = rest.split_once('.') else {
            let leaf = match mem_table {
                Some(t) if t.contains_key(rest) => rest.to_string(),
                Some(t) => {
                    let snake = rest.replace('-', "_");
                    if t.contains_key(&snake) {
                        snake
                    } else {
                        rest.to_string()
                    }
                }
                None => rest.to_string(),
            };
            out.push(leaf);
            return out;
        };

        let resolved = match mem_table {
            Some(t) if t.contains_key(seg) => seg.to_string(),
            Some(t) => {
                let snake = seg.replace('-', "_");
                if t.contains_key(&snake) {
                    snake
                } else {
                    seg.to_string()
                }
            }
            None => match doc_table {
                Some(dt) if dt.contains_key(seg) => seg.to_string(),
                Some(dt) => {
                    let snake = seg.replace('-', "_");
                    if dt.contains_key(&snake) {
                        snake
                    } else {
                        seg.to_string()
                    }
                }
                None => seg.to_string(),
            },
        };
        mem_table = mem_table
            .and_then(|t| t.get(&resolved))
            .and_then(toml::Value::as_table);
        doc_table = doc_table
            .and_then(|dt| dt.get(&resolved))
            .and_then(toml_edit::Item::as_table_like);
        out.push(resolved);
        rest = next_rest;
    }
}

fn lookup_path_in_table<'a>(root: &'a toml::Table, segs: &[&str]) -> Option<&'a toml::Value> {
    let mut current: Option<&toml::Value> = None;
    for (i, seg) in segs.iter().enumerate() {
        let table = if i == 0 { root } else { current?.as_table()? };
        current = table.get(*seg);
    }
    current
}

/// Read-only walk to the table-like node at `segs`, or `None` if any
/// segment is missing or not table-shaped on disk. Used to read a map-key
/// section's live on-disk keys without mutating the doc. `TableLike`
/// rather than `Table` because ZeroClaw loads (though never writes)
/// hand-edited inline tables — `openai = { "gpt-4.1" = { ... } }` parses
/// as `Item::Value(Value::InlineTable)`, which `as_table()` rejects; a
/// key living only in such a section must still resolve here or the
/// unresolvable-key bail in `apply_dirty_map_key_path` aborts the whole
/// `save_dirty` batch.
fn lookup_table_in_doc<'a>(
    root: &'a toml_edit::Table,
    segs: &[&str],
) -> Option<&'a dyn toml_edit::TableLike> {
    let mut cursor: &dyn toml_edit::TableLike = root;
    for seg in segs {
        cursor = cursor.get(seg)?.as_table_like()?;
    }
    Some(cursor)
}

/// `TableLike` rather than `Table` for the traversal cursor — same reason
/// as `lookup_table_in_doc` above: a map-key section may live inside a
/// hand-edited inline table (`openai = { "gpt-4.1" = { ... } }`), which
/// parses as `Item::Value(Value::InlineTable)`. `as_table_mut()` returns
/// `None` for that shape, so a `Table`-only cursor would silently return
/// without removing anything — `save_dirty` reports success while the key
/// stays on disk. `as_table_like_mut()` descends into both `Table` and
/// `InlineTable`, and `TableLike::remove` deletes from either.
fn delete_path_in_doc(root: &mut toml_edit::Table, segs: &[&str]) {
    let Some((last, parents)) = segs.split_last() else {
        return;
    };
    let mut cursor: &mut dyn toml_edit::TableLike = root;
    for seg in parents {
        cursor = match cursor.get_mut(seg).and_then(|i| i.as_table_like_mut()) {
            Some(t) => t,
            None => return,
        };
    }
    cursor.remove(last);
}

/// Same `TableLike` traversal as `delete_path_in_doc`, for the same
/// reason: a write into a key that already lives inside a hand-edited
/// inline table must land in that inline table rather than silently
/// no-op-ing because the cursor only understood standard `Table` nodes.
fn set_path_in_doc(root: &mut toml_edit::Table, segs: &[&str], value: &toml::Value) {
    let Some((last, parents)) = segs.split_last() else {
        return;
    };
    let mut cursor: &mut dyn toml_edit::TableLike = root;
    for seg in parents {
        if !cursor.contains_key(seg) {
            cursor.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        cursor = match cursor.get_mut(seg).and_then(|i| i.as_table_like_mut()) {
            Some(t) => t,
            None => return,
        };
    }
    let new_item = crate::migration::toml_value_to_edit_item(value);
    cursor.insert(last, new_item);
}

#[allow(clippy::unused_async)] // async needed on unix for tokio File I/O; no-op on other platforms
pub(crate) async fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = File::open(path).await.with_context(|| {
            format!(
                "Failed to open directory for fsync: {}",
                path.display().to_string()
            )
        })?;
        dir.sync_all().await.with_context(|| {
            format!(
                "Failed to fsync directory metadata: {}",
                path.display().to_string()
            )
        })?;
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .with_context(|| {
                format!(
                    "Failed to open directory for fsync: {}",
                    path.display().to_string()
                )
            })?;
        // FlushFileBuffers on directory handles returns ERROR_ACCESS_DENIED on
        // Windows (OS Error 5). This is expected — NTFS does not support
        // flushing directory metadata the same way Unix does. The individual
        // files have already been synced, so it is safe to ignore this error.
        if let Err(e) = dir.sync_all() {
            if e.raw_os_error() == Some(5) {
                ::zeroclaw_log::record!(
                    TRACE,
                    ::zeroclaw_log::Event::new(
                        crate::schema::CONFIG_SCHEMA_LOG_TARGET,
                        ::zeroclaw_log::Action::Note
                    ),
                    &format!(
                        "Ignoring expected ACCESS_DENIED when fsyncing directory on Windows: {}",
                        path.display().to_string()
                    )
                );
            } else {
                return Err(e).with_context(|| {
                    format!(
                        "Failed to fsync directory metadata: {}",
                        path.display().to_string()
                    )
                });
            }
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}
