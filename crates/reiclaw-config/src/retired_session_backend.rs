//! Fail-closed admission for the retired session-backend selector.
//!
//! Only the former SQLite default can be ignored. Selecting JSONL or an
//! unknown store must never silently expose an empty replacement history.

use anyhow::Result;
use toml::Value;

const PATH: &str = "channels.session_backend";
const ENV_TAIL: &str = "channels__session_backend";

/// A pre-write refusal that never includes an operator-supplied value.
#[derive(Debug)]
pub struct RetiredSessionBackendError;

impl std::fmt::Display for RetiredSessionBackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "channels.session_backend is retired: SQLite is the only session backend. \
             Review the existing session history and explicitly remove this setting \
             and its REI_channels__session_backend override. Only the former sqlite \
             default may be ignored; JSONL, unknown and malformed selections are \
             refused. Legacy JSONL startup import remains available, but this \
             refusal does not migrate or delete session files.",
        )
    }
}

impl std::error::Error for RetiredSessionBackendError {}

fn is_old_default(value: &Value) -> bool {
    value.as_str() == Some("sqlite")
}

pub(crate) fn reject_retired_session_backend_config(root: &Value) -> Result<()> {
    if let Some(value) = root
        .get("channels")
        .and_then(|channels| channels.get("session_backend"))
        && !is_old_default(value)
    {
        return Err(RetiredSessionBackendError.into());
    }
    Ok(())
}

/// File warnings are collected before admission. Do not describe a refused
/// selection as ignored, or expose its contents in the warning.
pub(crate) fn permits_field_warning(path: &str, root: &toml::Table) -> bool {
    path != PATH
        || root
            .get("channels")
            .and_then(|channels| channels.get("session_backend"))
            .is_some_and(is_old_default)
}

/// Apply before the generic warning-only tombstone. An env string can be bare
/// or a quoted TOML string, but a child path can never name a backend value.
pub(crate) fn reject_retired_session_backend_env(tail: &str, raw: &str) -> Result<()> {
    if tail == ENV_TAIL {
        if raw == "sqlite" {
            return Ok(());
        }
        // Discard parse errors because they can quote arbitrary submitted data.
        let parsed = toml::from_str::<toml::Table>(&format!("value = {raw}"));
        if let Ok(table) = parsed
            && table.len() == 1
            && table.get("value").is_some_and(is_old_default)
        {
            return Ok(());
        }
        return Err(RetiredSessionBackendError.into());
    }
    if tail
        .strip_prefix(ENV_TAIL)
        .is_some_and(|suffix| suffix.starts_with("__"))
    {
        return Err(RetiredSessionBackendError.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::current::{parse_current, parse_current_resilient};

    #[test]
    fn retired_session_backend_accepts_only_absent_or_sqlite_default() {
        let absent = "schema_version = 3\n[channels]\nshow_tool_calls = true\n";
        let explicit = format!("{absent}session_backend = 'sqlite'\n");
        for raw in [absent, &explicit] {
            let config = parse_current(raw).unwrap();
            assert!(config.channels.show_tool_calls);
            assert!(!config.prop_paths().iter().any(|path| path == PATH));
            assert!(
                !toml::to_string(&config)
                    .unwrap()
                    .contains("session_backend")
            );
            let resilient = parse_current_resilient(raw).unwrap();
            assert!(resilient.degraded_sections.is_empty());
            assert!(resilient.channels.show_tool_calls);
        }
        assert!(crate::validation_warnings::retired_field_tombstones(absent).is_empty());
        let warnings = crate::validation_warnings::retired_field_tombstones(&explicit);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].path, PATH);
        assert_eq!(warnings[0].code, "session_backend_selector_removed");
        let generated = crate::generate::generate(&Default::default()).unwrap();
        assert!(!generated.contains("session_backend"));
    }

    #[test]
    fn retired_session_backend_refuses_before_section_salvage() {
        for value in [
            "'jsonl'",
            "'private-unknown-backend'",
            "'default'",
            "''",
            "true",
            "7",
            "[]",
            "{ sqlite = true }",
        ] {
            // A second malformed field would normally trigger channel salvage.
            let raw = format!(
                "schema_version = 3\n[channels]\nsession_backend = {value}\nshow_tool_calls = 'invalid'\n"
            );
            assert!(crate::validation_warnings::retired_field_tombstones(&raw).is_empty());
            for result in [parse_current(&raw), parse_current_resilient(&raw)] {
                let error = result.unwrap_err();
                assert!(error.is::<RetiredSessionBackendError>(), "{error:#}");
                assert!(!format!("{error:#}").contains("private-unknown-backend"));
            }
        }
    }

    #[tokio::test]
    async fn retired_session_backend_save_and_comment_paths_preserve_operator_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let mut config = crate::schema::Config {
            config_path: path.clone(),
            data_dir: temp.path().join("data"),
            loaded_from: Some(path.clone()),
            ..Default::default()
        };
        config.channels.show_tool_calls = true;
        config.mark_dirty("channels.show_tool_calls");
        for value in ["'jsonl'", "'private-unknown-backend'", "false"] {
            let raw = format!(
                "# operator history choice\nschema_version = 3\n[channels]\nsession_backend = {value}\n"
            );
            tokio::fs::write(&path, &raw).await.unwrap();
            for mode in ["save", "save_dirty", "force_save"] {
                let result = match mode {
                    "save" => config.save().await,
                    "save_dirty" => config.save_dirty().await,
                    _ => config.force_save().await,
                };
                let error = result.unwrap_err();
                assert!(
                    error.is::<RetiredSessionBackendError>(),
                    "{mode}: {error:#}"
                );
                assert!(!format!("{error:#}").contains("private-unknown-backend"));
                assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), raw);
            }
            let error = crate::comment_writer::apply_comments(
                &path,
                &[("schema_version".into(), "updated".into())],
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(!error.to_string().contains("private-unknown-backend"));
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), raw);
            assert!(!temp.path().join("data/sessions/sessions.db").exists());
        }
    }
}
