//! ModelProvider alias functions used by config validation.
//! These are extracted from the model_providers module to break the circular
//! dependency between config and model_providers.

pub fn is_minimax_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "minimax"
            | "minimax-intl"
            | "minimax-io"
            | "minimax-global"
            | "minimax-oauth"
            | "minimax-portal"
            | "minimax-oauth-global"
            | "minimax-portal-global"
    )
}

pub fn is_minimax_cn_alias(name: &str) -> bool {
    matches!(
        name,
        "minimax-cn" | "minimaxi" | "minimax-oauth-cn" | "minimax-portal-cn"
    )
}

pub fn is_minimax_alias(name: &str) -> bool {
    is_minimax_intl_alias(name) || is_minimax_cn_alias(name)
}

pub fn is_qwen_cn_alias(name: &str) -> bool {
    matches!(name, "qwen" | "dashscope" | "qwen-cn" | "dashscope-cn")
}

pub fn is_qwen_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "qwen-intl" | "dashscope-intl" | "qwen-international" | "dashscope-international"
    )
}

pub fn is_qwen_us_alias(name: &str) -> bool {
    matches!(name, "qwen-us" | "dashscope-us")
}

pub fn is_qwen_oauth_alias(name: &str) -> bool {
    matches!(name, "qwen-code" | "qwen-oauth" | "qwen_oauth")
}

pub fn is_bailian_alias(name: &str) -> bool {
    matches!(name, "bailian" | "aliyun-bailian" | "aliyun")
}

pub fn is_qwen_alias(name: &str) -> bool {
    is_qwen_cn_alias(name)
        || is_qwen_intl_alias(name)
        || is_qwen_us_alias(name)
        || is_qwen_oauth_alias(name)
}

pub fn canonical_china_provider_name(name: &str) -> Option<&'static str> {
    if is_qwen_alias(name) {
        Some("qwen")
    } else if is_minimax_alias(name) {
        Some("minimax")
    } else if is_bailian_alias(name) {
        Some("bailian")
    } else {
        None
    }
}

pub fn family_honors_wire_api(family: &str) -> bool {
    matches!(family, "openai" | "llamacpp" | "custom")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimax_aliases_split_intl_and_cn() {
        for name in [
            "minimax",
            "minimax-intl",
            "minimax-io",
            "minimax-global",
            "minimax-oauth",
            "minimax-portal",
            "minimax-oauth-global",
            "minimax-portal-global",
        ] {
            assert!(is_minimax_intl_alias(name));
            assert!(is_minimax_alias(name));
        }
        for name in [
            "minimax-cn",
            "minimaxi",
            "minimax-oauth-cn",
            "minimax-portal-cn",
        ] {
            assert!(is_minimax_cn_alias(name));
            assert!(is_minimax_alias(name));
        }
        assert!(!is_minimax_alias("minimax-unknown"));
    }

    #[test]
    fn qwen_alias_family_is_union_without_bailian() {
        assert!(is_qwen_cn_alias("qwen"));
        assert!(is_qwen_cn_alias("dashscope"));
        assert!(is_qwen_intl_alias("qwen-intl"));
        assert!(is_qwen_intl_alias("dashscope-international"));
        assert!(is_qwen_us_alias("qwen-us"));
        assert!(is_qwen_oauth_alias("qwen-code"));
        for name in [
            "qwen",
            "dashscope",
            "qwen-intl",
            "qwen-us",
            "qwen-code",
            "qwen_oauth",
        ] {
            assert!(is_qwen_alias(name));
        }
        // bailian is its own family and must not fold into qwen.
        assert!(!is_qwen_alias("bailian"));
        assert!(!is_qwen_alias("aliyun"));
    }

    #[test]
    fn standalone_china_aliases_match() {
        assert!(is_bailian_alias("bailian"));
        assert!(is_bailian_alias("aliyun"));
    }

    #[test]
    fn canonical_name_maps_each_china_family() {
        let cases = [
            ("qwen", "qwen"),
            ("dashscope", "qwen"),
            ("qwen-code", "qwen"),
            ("minimax", "minimax"),
            ("minimaxi", "minimax"),
            ("bailian", "bailian"),
            ("aliyun", "bailian"),
        ];
        for (alias, canonical) in cases {
            assert_eq!(
                canonical_china_provider_name(alias),
                Some(canonical),
                "{alias} should canonicalize to {canonical}",
            );
        }
        assert_eq!(canonical_china_provider_name("openai"), None);
        assert_eq!(canonical_china_provider_name("anthropic"), None);
        assert_eq!(canonical_china_provider_name(""), None);
    }

    #[test]
    fn canonical_name_agrees_with_family_predicates() {
        let samples = [
            "qwen",
            "dashscope-intl",
            "minimax",
            "minimax-cn",
            "bailian",
            "aliyun",
        ];
        for name in samples {
            let canonical = canonical_china_provider_name(name)
                .unwrap_or_else(|| panic!("{name} should be recognized as a China provider"));
            match canonical {
                "qwen" => assert!(is_qwen_alias(name)),
                "minimax" => assert!(is_minimax_alias(name)),
                "bailian" => assert!(is_bailian_alias(name)),
                other => panic!("unexpected canonical name {other} for {name}"),
            }
        }
    }

    #[test]
    fn wire_api_honored_only_by_byo_endpoint_families() {
        for family in ["openai", "llamacpp", "custom"] {
            assert!(family_honors_wire_api(family), "{family} honors wire_api");
        }
        for family in [
            "anthropic",
            "qwen",
            "glm",
            "moonshot",
            "gemini",
            "minimax",
            "",
        ] {
            assert!(!family_honors_wire_api(family), "{family} ignores wire_api");
        }
    }
}
