pub use reiclaw_config::scattered_types::{AutoClassifyConfig, EvalConfig};

// ── Complexity estimation ───────────────────────────────────────

/// Coarse complexity tier for a user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplexityTier {
    /// Short, simple query (greetings, yes/no, lookups).
    Simple,
    /// Typical request — not trivially simple, not deeply complex.
    Standard,
    /// Long or reasoning-heavy request (code, multi-step, analysis).
    Complex,
}

/// Heuristic keywords that signal reasoning complexity.
const REASONING_KEYWORDS: &[&str] = &[
    "explain",
    "why",
    "analyze",
    "compare",
    "design",
    "implement",
    "refactor",
    "debug",
    "optimize",
    "architecture",
    "trade-off",
    "tradeoff",
    "reasoning",
    "step by step",
    "think through",
    "evaluate",
    "critique",
    "pros and cons",
];

pub fn estimate_complexity(message: &str) -> ComplexityTier {
    let lower = message.to_lowercase();
    let len = message.len();

    let keyword_count = REASONING_KEYWORDS
        .iter()
        .filter(|kw| lower.contains(**kw))
        .count();

    let has_code_fence = message.contains("```");

    if len > 200 || has_code_fence || keyword_count >= 2 {
        return ComplexityTier::Complex;
    }

    if len < 50 && keyword_count == 0 {
        return ComplexityTier::Simple;
    }

    ComplexityTier::Standard
}

// ── Auto-classify helper ────────────────────────────────────────

/// Map a complexity tier to the configured hint, if any.
pub fn hint_for_tier(cfg: &AutoClassifyConfig, tier: ComplexityTier) -> Option<&str> {
    match tier {
        ComplexityTier::Simple => cfg.simple_hint.as_deref(),
        ComplexityTier::Standard => cfg.standard_hint.as_deref(),
        ComplexityTier::Complex => cfg.complex_hint.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── estimate_complexity ─────────────────────────────────────

    #[test]
    fn simple_short_message() {
        assert_eq!(estimate_complexity("hi"), ComplexityTier::Simple);
        assert_eq!(estimate_complexity("hello"), ComplexityTier::Simple);
        assert_eq!(estimate_complexity("yes"), ComplexityTier::Simple);
    }

    #[test]
    fn complex_long_message() {
        let long = "a".repeat(201);
        assert_eq!(estimate_complexity(&long), ComplexityTier::Complex);
    }

    #[test]
    fn complex_code_fence() {
        let msg = "Here is some code:\n```rust\nfn main() {}\n```";
        assert_eq!(estimate_complexity(msg), ComplexityTier::Complex);
    }

    #[test]
    fn complex_multiple_reasoning_keywords() {
        let msg = "Please explain why this design is better and analyze the trade-off";
        assert_eq!(estimate_complexity(msg), ComplexityTier::Complex);
    }

    #[test]
    fn standard_medium_message() {
        // 50+ chars but no code fence, < 2 reasoning keywords
        let msg = "Can you help me find a good restaurant in this area please?";
        assert_eq!(estimate_complexity(msg), ComplexityTier::Standard);
    }

    #[test]
    fn standard_short_with_one_keyword() {
        // < 50 chars but has 1 reasoning keyword → still not Simple
        let msg = "explain this";
        assert_eq!(estimate_complexity(msg), ComplexityTier::Standard);
    }

    // ── auto_classify ───────────────────────────────────────────

    #[test]
    fn auto_classify_maps_tiers_to_hints() {
        let ac = AutoClassifyConfig {
            simple_hint: Some("fast".into()),
            standard_hint: None,
            complex_hint: Some("reasoning".into()),
        };
        assert_eq!(hint_for_tier(&ac, ComplexityTier::Simple), Some("fast"));
        assert_eq!(hint_for_tier(&ac, ComplexityTier::Standard), None);
        assert_eq!(
            hint_for_tier(&ac, ComplexityTier::Complex),
            Some("reasoning")
        );
    }
}
