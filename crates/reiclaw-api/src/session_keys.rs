//! Session key normalization shared across infra and memory backends.
//!
//! The same normalized identifier addresses persisted session history, the
//! conversation cache and `Memory::store`/`Memory::recall` session filters.
//! Preserve this mapping across restarts and when importing historical JSONL
//! filenames into SQLite; retiring the file writer must not rename sessions.

/// Replace every character outside `[A-Za-z0-9_-]` with `_`. Idempotent.
///
/// Callers building session keys must pre-apply this so the runtime cache,
/// persisted history and `session_id` column in memory backends all agree.
pub fn sanitize_session_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_special_characters_with_underscore() {
        assert_eq!(
            sanitize_session_key("slack_C123_1.2_user one"),
            "slack_C123_1_2_user_one"
        );
    }

    #[test]
    fn preserves_alphanumeric_underscore_and_hyphen() {
        let key = "abc-DEF_123";
        assert_eq!(sanitize_session_key(key), key);
    }

    #[test]
    fn is_idempotent() {
        let once = sanitize_session_key("whatsapp_123@g.us_alice");
        let twice = sanitize_session_key(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn handles_empty_string() {
        assert_eq!(sanitize_session_key(""), "");
    }

    #[test]
    fn preserves_unicode_alphanumeric() {
        // is_alphanumeric() treats unicode letters/digits as alphanumeric.
        assert_eq!(sanitize_session_key("user_Алиса"), "user_Алиса");
    }

    #[test]
    fn replaces_path_separator_characters() {
        assert_eq!(
            sanitize_session_key("../tenant\\user/channel"),
            "___tenant_user_channel"
        );
    }
}
