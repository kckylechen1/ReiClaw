//! Generate an example of the current schema without historical migration lenses.
use crate::schema::Config;
use anyhow::{Context, Result};
use std::path::Path;

const CURRENT_FIXTURE: &str = include_str!("../fixtures/v3.toml");

#[derive(Debug, Default, Clone)]
pub struct GenerateOptions<'a> {
    pub encrypt_secrets: bool,
    pub secret_store_dir: Option<&'a Path>,
}

/// Generate the supported V3 schema. User input is never stripped here.
pub fn generate(opts: &GenerateOptions<'_>) -> Result<String> {
    let mut value: toml::Value =
        toml::from_str(CURRENT_FIXTURE).context("embedded current-schema fixture is malformed")?;
    crate::current::require_current_schema(&value)?;
    strip_retired_surfaces(&mut value);
    let config: Config = value
        .clone()
        .try_into()
        .context("generated config does not match the current schema")?;
    config
        .validate()
        .context("generated current-schema config is invalid")?;
    // Serialize only the live typed schema; historical passthrough keys in a
    // hand-maintained example must never become advertised configuration.
    value = toml::Value::try_from(config).context("failed to serialize current example")?;
    if opts.encrypt_secrets {
        let dir = opts
            .secret_store_dir
            .context("--encrypt requires a secret-store directory")?;
        let store = crate::secrets::SecretStore::new(dir, true);
        encrypt_secret_strings(&mut value, &store)?;
    }
    toml::to_string_pretty(&value).context("failed to serialize generated config")
}

fn secret_key_names() -> &'static std::collections::HashSet<&'static str> {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static CACHE: OnceLock<HashSet<&'static str>> = OnceLock::new();
    CACHE.get_or_init(|| Config::secret_field_terminals().into_iter().collect())
}

/// Remove retired config sections (by dotted path, from
/// [`crate::validation_warnings::RETIRED_CONFIG_SURFACES`]) from a
/// generated config value. Drops only the final segment of each retired
/// path when its parent chain fully resolves, so adjacent live keys are
/// untouched and a missing parent is a no-op.
///
/// Retired fields ([`crate::validation_warnings::RETIRED_CONFIG_FIELDS`])
/// are dropped the same way, and a key whose value names a retired backend
/// ([`crate::validation_warnings::RETIRED_CONFIG_VALUES`]) is dropped so it
/// falls back to its default instead of carrying the retired value forward.
fn strip_retired_surfaces(value: &mut toml::Value) {
    for (path, _) in crate::validation_warnings::RETIRED_CONFIG_SURFACES {
        let segments: Vec<&str> = path.split('.').collect();
        remove_dotted(value, &segments, &|_| true);
    }
    for (path, _) in crate::validation_warnings::RETIRED_CONFIG_FIELDS {
        let segments: Vec<&str> = path.split('.').collect();
        remove_dotted(value, &segments, &|_| true);
    }
    for (path, retired, _) in crate::validation_warnings::RETIRED_CONFIG_VALUES {
        let segments: Vec<&str> = path.split('.').collect();
        remove_dotted(value, &segments, &|leaf| {
            leaf.as_str()
                .is_some_and(|v| crate::validation_warnings::is_retired_value(v, retired))
        });
    }
}

/// Recursive descent so each step owns exactly one mutable borrow; drops
/// only the final segment when the parent chain fully resolves and the leaf
/// passes `should_remove`. A `*` segment matches every key of that table.
fn remove_dotted(
    value: &mut toml::Value,
    segments: &[&str],
    should_remove: &dyn Fn(&toml::Value) -> bool,
) {
    let Some((first, rest)) = segments.split_first() else {
        return;
    };
    let Some(table) = value.as_table_mut() else {
        return;
    };
    if *first == "*" {
        for (_, child) in table.iter_mut() {
            remove_dotted(child, rest, should_remove);
        }
    } else if rest.is_empty() {
        if table.get(*first).is_some_and(should_remove) {
            table.remove(*first);
        }
    } else if let Some(child) = table.get_mut(*first) {
        remove_dotted(child, rest, should_remove);
    }
}

pub fn encrypt_secret_strings(
    value: &mut toml::Value,
    store: &crate::secrets::SecretStore,
) -> Result<()> {
    let names = secret_key_names();
    encrypt_walk(value, store, names)
}

fn encrypt_walk(
    value: &mut toml::Value,
    store: &crate::secrets::SecretStore,
    names: &std::collections::HashSet<&'static str>,
) -> Result<()> {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                if names.contains(key.as_str()) {
                    encrypt_in_place(child, store)
                        .with_context(|| format!("encrypting secret at key `{key}`"))?;
                } else {
                    encrypt_walk(child, store, names)?;
                }
            }
        }
        toml::Value::Array(items) => {
            for item in items.iter_mut() {
                encrypt_walk(item, store, names)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn encrypt_in_place(value: &mut toml::Value, store: &crate::secrets::SecretStore) -> Result<()> {
    match value {
        toml::Value::String(s)
            if !crate::secrets::SecretStore::is_encrypted(s) && !s.is_empty() =>
        {
            let encrypted = store.encrypt(s).context("encrypt string")?;
            *s = encrypted;
        }
        toml::Value::Array(items) => {
            for item in items.iter_mut() {
                encrypt_in_place(item, store)?;
            }
        }
        toml::Value::Table(table) => {
            for (_, child) in table.iter_mut() {
                encrypt_in_place(child, store)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_retired_surfaces_removes_every_table_entry_and_keeps_siblings() {
        // Discriminator: the seed fixture no longer carries the retired
        // sections, so `generate_never_emits_retired_config_surfaces` in the
        // integration suite cannot see a stripping regression. Exercise the
        // strip boundary directly with every retired path injected, and
        // require live siblings along the parent chain to survive.
        for (path, _) in crate::validation_warnings::RETIRED_CONFIG_SURFACES {
            let segments: Vec<&str> = path.split('.').collect();
            let mut doc = String::new();
            for depth in 1..=segments.len() {
                doc.push_str(&format!(
                    "[{}]\nlive_sibling = 1\n",
                    segments[..depth].join(".")
                ));
            }
            let mut value: toml::Value = toml::from_str(&doc).unwrap();
            strip_retired_surfaces(&mut value);
            let mut table = value.as_table().unwrap();
            for (i, seg) in segments.iter().enumerate() {
                if i == segments.len() - 1 {
                    assert!(
                        !table.contains_key(*seg),
                        "retired section {path} must be stripped from generated output"
                    );
                } else {
                    table = table.get(*seg).unwrap().as_table().unwrap();
                    assert!(
                        table.contains_key("live_sibling"),
                        "live sibling beside retired segment {seg} must survive"
                    );
                }
            }
        }
    }
}
