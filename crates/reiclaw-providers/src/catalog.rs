//! Per-family catalog source table.

use anyhow::Result;

/// `(models.dev key, openrouter.ai vendor prefix)` for a family name.
/// Either or both can be `None` for families with no public catalog
/// (local-only servers, credential-required APIs without a public
/// `/models` index).
#[must_use]
pub fn catalog_source_for(family: &str) -> Option<(Option<&'static str>, Option<&'static str>)> {
    let pair: (Option<&'static str>, Option<&'static str>) = match family {
        // First-party / bespoke factories.
        "openai" => (Some("openai"), Some("openai")),
        "anthropic" => (Some("anthropic"), Some("anthropic")),
        "gemini" => (Some("google"), Some("google")),
        "minimax" => (Some("minimax"), Some("minimax")),
        "lmstudio" => (Some("lmstudio"), None),
        "ovh" => (Some("ovhcloud"), None),
        "qwen" => (Some("alibaba"), Some("qwen")),
        "groq" => (Some("groq"), None),
        // Families with no public catalog: local-only servers (no public
        // /models index without a running server) or credential-required
        // APIs with no published catalog. Operator pastes a credential and
        // the provider's `/models` endpoint serves the list directly.
        "osaurus" | "llamacpp" | "ollama" | "custom" => (None, None),
        _ => return None,
    };
    Some(pair)
}

/// Probe the catalog for `family` without constructing a live provider.
/// Returns the union of every known public catalog source. Errors if
/// `family` is unknown or has no public catalog source set.
pub async fn list_models_for_family(family: &str) -> Result<Vec<String>> {
    let Some((md_key, or_prefix)) = catalog_source_for(family) else {
        anyhow::bail!("unknown provider family {family:?}");
    };
    if let Some(k) = md_key
        && let Ok(ms) = crate::models_dev::list_models_for(k).await
        && !ms.is_empty()
    {
        return Ok(ms);
    }
    if let Some(p) = or_prefix {
        return crate::openrouter_catalog::list_models_for_vendor(p).await;
    }
    anyhow::bail!("no public catalog for family {family:?}")
}

#[must_use]
pub fn sort_model_catalog_for_chat(provider: &str, models: Vec<String>) -> Option<Vec<String>> {
    let provider_l = provider.to_ascii_lowercase();
    let mut ranked: Vec<(i32, String, String)> = models
        .into_iter()
        .filter_map(|model| {
            let model_l = model.to_ascii_lowercase();
            if is_non_chat_model_lower(&model_l) {
                None
            } else {
                Some((chat_model_rank_lower(&provider_l, &model_l), model_l, model))
            }
        })
        .collect();
    if ranked.is_empty() {
        return None;
    }
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    Some(ranked.into_iter().map(|(_, _, model)| model).collect())
}

fn chat_model_rank_lower(provider: &str, model_l: &str) -> i32 {
    let mut rank = 100;

    if provider.starts_with("openai") {
        if model_l.starts_with("gpt-5") {
            rank -= 70;
        } else if model_l.starts_with("gpt-4.1")
            || model_l.starts_with("gpt-4o")
            || model_l.starts_with("o3")
            || model_l.starts_with("o4")
        {
            rank -= 55;
        } else if model_l.starts_with("gpt-3.5") {
            rank -= 10;
        }
    } else if contains_any(
        model_l,
        &[
            "claude", "sonnet", "opus", "gpt", "gemini", "deepseek", "qwen", "kimi", "llama",
            "mistral", "grok", "coder", "code", "reason", "r1", "o3", "o4",
        ],
    ) {
        rank -= 45;
    }

    rank
}

fn is_non_chat_model_lower(model: &str) -> bool {
    contains_any(
        model,
        &[
            "image",
            "audio",
            "tts",
            "transcribe",
            "embedding",
            "moderation",
            "realtime",
            "whisper",
        ],
    )
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_canonical_family_has_a_catalog_table_entry() {
        macro_rules! collect_family_names {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                vec![$($type_str),+]
            };
        }
        let families: Vec<&str> =
            reiclaw_config::for_each_model_provider_slot!(collect_family_names);
        let mut missing: Vec<&str> = Vec::new();
        for family in &families {
            if catalog_source_for(family).is_none() {
                missing.push(family);
            }
        }
        assert!(
            missing.is_empty(),
            "catalog_source_for is missing entries for: {missing:?}"
        );
    }

    #[test]
    fn unknown_family_returns_none() {
        assert!(catalog_source_for("not_a_real_provider").is_none());
    }

    #[test]
    fn known_family_with_dual_sources_returns_both() {
        let (md, or) = catalog_source_for("openai").expect("openai is canonical");
        assert_eq!(md, Some("openai"));
        assert_eq!(or, Some("openai"));
    }

    #[test]
    fn local_only_family_returns_no_sources() {
        let (md, or) = catalog_source_for("llamacpp").expect("llamacpp is canonical");
        assert_eq!(md, None);
        assert_eq!(or, None);
    }

    #[test]
    fn bespoke_family_with_only_models_dev() {
        let (md, or) = catalog_source_for("lmstudio").expect("lmstudio is canonical");
        assert_eq!(md, Some("lmstudio"));
        assert_eq!(or, None);
    }

    #[test]
    fn quickstart_sort_prefers_chat_and_coding_models() {
        let sorted = sort_model_catalog_for_chat(
            "openai",
            vec![
                "chatgpt-image-latest".into(),
                "text-embedding-ada-002".into(),
                "gpt-3.5-turbo".into(),
                "gpt-5".into(),
                "tts-1".into(),
            ],
        )
        .expect("chat model catalog");

        assert_eq!(sorted[0], "gpt-5");
        assert!(!sorted.iter().any(|m| m == "chatgpt-image-latest"));
        assert!(!sorted.iter().any(|m| m == "text-embedding-ada-002"));
        assert!(!sorted.iter().any(|m| m == "tts-1"));

        let sorted = sort_model_catalog_for_chat(
            "openrouter",
            vec![
                "vendor/plain-model".into(),
                "openai/gpt-4.1".into(),
                "some/image-model".into(),
                "anthropic/claude-sonnet-4".into(),
            ],
        )
        .expect("chat model catalog");
        assert_eq!(sorted[0], "anthropic/claude-sonnet-4");
        assert_eq!(sorted[1], "openai/gpt-4.1");
        assert!(!sorted.iter().any(|m| m == "some/image-model"));
    }

    #[test]
    fn quickstart_sort_returns_none_when_only_non_chat_models_exist() {
        let sorted = sort_model_catalog_for_chat(
            "openai",
            vec!["gpt-image-1.5".into(), "text-embedding-ada-002".into()],
        );

        assert!(sorted.is_none());
    }
}
