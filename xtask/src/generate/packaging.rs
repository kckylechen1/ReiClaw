//! Packaging surface renderers: AUR PKGBUILD (shell-comment sentinels). Version
//! and feature sets come from the canonical spec; nothing is typed.

use super::spec::{self, Selection};
use std::path::Path;

fn begin(zone: &str) -> String {
    format!("# >>> generated:{zone} by `cargo generate installers` - do not edit <<<")
}
fn end(zone: &str) -> String {
    format!("# >>> end generated:{zone} <<<")
}

fn splice(current: &str, zone: &str, body: &str) -> anyhow::Result<String> {
    let b = begin(zone);
    let e = end(zone);
    let begin_at = current
        .find(&b)
        .ok_or_else(|| anyhow::Error::msg(format!("missing generated:{zone} BEGIN sentinel")))?;
    let after_begin = begin_at + b.len();
    let end_rel = current[after_begin..]
        .find(&e)
        .ok_or_else(|| anyhow::Error::msg(format!("missing generated:{zone} END sentinel")))?;
    let end_at = after_begin + end_rel;
    let mut out = String::new();
    out.push_str(&current[..after_begin]);
    out.push('\n');
    out.push_str(body);
    out.push('\n');
    out.push_str(&current[end_at..]);
    Ok(out)
}

/// PKGBUILD: regenerate the `pkgver` and the build `--features` from the spec.
/// Ships `Selection::Dist`. Two zones - version and the cargo build line.
pub fn render_pkgbuild(root: &Path, current: &str) -> anyhow::Result<String> {
    let version = spec::resolve_version(root)?;
    let features = spec::resolve_feature_list(root, &Selection::Dist)?.join(",");
    let with_ver = splice(current, "pkgbuild-version", &format!("pkgver={version}"))?;
    splice(
        &with_ver,
        "pkgbuild-build",
        &format!("  cargo build --frozen --profile dist --features {features}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self as ts, Cause};
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn pkgbuild_version_matches_workspace() {
        let v = spec::resolve_version(&root()).unwrap();
        let cur = format!(
            "a\n{}\npkgver=0.0.0\n{}\nb\n",
            begin("pkgbuild-version"),
            end("pkgbuild-version")
        );
        let out = splice(&cur, "pkgbuild-version", &format!("pkgver={v}")).unwrap();
        assert!(out.contains(&format!("pkgver={v}")));
        assert!(!out.contains("pkgver=0.0.0"));
    }

    #[test]
    fn pkgbuild_features_are_lean_dist_channels() {
        let f = spec::resolve_feature_list(&root(), &Selection::Dist)
            .unwrap()
            .join(",");
        // #584: sample the protected surfaces from the live registry.
        // An empty sample must never make the walk pass vacuously.
        let protected = ts::protected_retained_channel_features();
        ts::require_non_empty("protected sample", Cause::Protected, protected.len());
        for feature in protected {
            assert!(
                f.contains(&feature),
                "pkgbuild dist must ship the protected channel feature {feature}"
            );
        }
        // Removed channels stay absent forever.
        assert!(!f.contains("channel-matrix"));
        assert!(!f.contains("channel-slack"));
        assert!(!f.contains("whatsapp-web"));
    }
}
