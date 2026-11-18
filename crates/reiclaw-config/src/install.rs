//! Idempotent repair of the retained V3 shared-skills directory layout.

use anyhow::{Context, Result};
use std::path::Path;

/// Heal intermediate installs that landed skills under
/// `agents/default/workspace/skills/` before the host-wide
/// `shared/skills/` layout was introduced. Idempotent.
pub(crate) fn relocate_default_agent_skills_to_shared(install_root: &Path) -> Result<bool> {
    let src = install_root
        .join("agents")
        .join("default")
        .join("workspace")
        .join("skills");
    let dst = install_root.join("shared").join("skills");
    if !src.is_dir() {
        return Ok(false);
    }
    let dst_populated = dst
        .is_dir()
        .then(|| std::fs::read_dir(&dst).ok())
        .flatten()
        .is_some_and(|mut it| it.next().is_some());
    if dst_populated {
        return Ok(false);
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "[system] failed to create shared workspace parent {}",
                parent.display()
            )
        })?;
    }
    if std::fs::rename(&src, &dst).is_err() {
        copy_dir_recursive(&src, &dst).with_context(|| {
            format!(
                "[system] failed to copy {} to {}",
                src.display(),
                dst.display()
            )
        })?;
        std::fs::remove_dir_all(&src)
            .with_context(|| format!("[system] failed to remove {} after copy", src.display()))?;
    }
    ::reiclaw_log::record!(
        INFO,
        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "from": src.display().to_string(),
                "to": dst.display().to_string(),
            })
        ),
        "[system] layout repair: lifted default-agent skills into shared/"
    );
    Ok(true)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ft.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(&from)?;
                std::os::unix::fs::symlink(&target, &to)?;
            }
            #[cfg(not(unix))]
            {
                std::fs::copy(&from, &to)?;
            }
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_skills_layout_repair_preserves_content_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("agents/default/workspace/skills/example");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "current skill").unwrap();
        assert!(relocate_default_agent_skills_to_shared(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("shared/skills/example/SKILL.md")).unwrap(),
            "current skill"
        );
        assert!(!src.exists());
        assert!(!relocate_default_agent_skills_to_shared(dir.path()).unwrap());
    }

    #[test]
    fn populated_shared_skills_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("agents/default/workspace/skills");
        let dst = dir.path().join("shared/skills");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("SKILL.md"), "prior skill").unwrap();
        std::fs::write(dst.join("SKILL.md"), "shared skill").unwrap();
        assert!(!relocate_default_agent_skills_to_shared(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(src.join("SKILL.md")).unwrap(),
            "prior skill"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("SKILL.md")).unwrap(),
            "shared skill"
        );
    }
}
