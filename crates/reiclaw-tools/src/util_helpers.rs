/// Truncate a string to `max_chars` Unicode characters, appending "..." if truncated.
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}...", s[..idx].trim_end()),
        None => s.to_string(),
    }
}

/// Utility enum for handling optional values in config set/unset operations.
pub enum MaybeSet<T> {
    Set(T),
    Unset,
    Null,
}

#[cfg(test)]
pub(crate) fn workspace_prefixed_relative_path_for_test(
    workspace: &std::path::Path,
) -> std::path::PathBuf {
    let mut relative = std::path::PathBuf::new();
    for component in workspace.components() {
        match component {
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                panic!("test workspace path must not contain parent components")
            }
            std::path::Component::Normal(part) => relative.push(part),
        }
    }
    relative
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_with_ellipsis_keeps_short_or_exact_strings() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
        // Exactly at the char budget is not truncated.
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
        assert_eq!(truncate_with_ellipsis("", 3), "");
    }

    #[test]
    fn truncate_with_ellipsis_truncates_and_appends() {
        assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_with_ellipsis_trims_trailing_space_before_ellipsis() {
        // The kept slice "ab " is trimmed before the ellipsis is appended.
        assert_eq!(truncate_with_ellipsis("ab cd", 3), "ab...");
    }

    #[test]
    fn truncate_with_ellipsis_counts_unicode_chars_not_bytes() {
        // "héllo" cut after 2 chars keeps "hé" (3 bytes), not 2 bytes.
        assert_eq!(truncate_with_ellipsis("héllo", 2), "hé...");
    }
}
