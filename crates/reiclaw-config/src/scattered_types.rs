//! Config types that were originally defined in their home modules (agent, channels, tools, trust)
//! but are needed by the config schema. Moved here to break circular dependencies.

use crate::traits::{HasPropKind, PropKind};
use reiclaw_macros::Configurable;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Agent config types ──────────────────────────────────────────

/// How deeply the model should reason for a given message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Max,
}

impl HasPropKind for ThinkingLevel {
    const PROP_KIND: PropKind = PropKind::Enum;
}

impl ThinkingLevel {
    pub fn from_str_insensitive(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "off" | "none" => Some(Self::Off),
            "minimal" | "min" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" | "med" | "default" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" | "maximum" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    pub fn default_budget_tokens(&self) -> Option<u32> {
        match self {
            Self::Off | Self::Minimal | Self::Low | Self::Medium => None,
            Self::High => Some(10_000),
            Self::Max => Some(50_000),
        }
    }
}

pub use reiclaw_api::model_provider::{MAX_BUDGET_TOKENS, MIN_BUDGET_TOKENS, NativeThinkingParams};

/// Configuration for thinking/reasoning level control.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "agent.thinking"]
pub struct ThinkingConfig {
    #[serde(default)]
    pub default_level: ThinkingLevel,
    /// Enables provider-native thinking parameters when the selected level has a budget.
    #[serde(default)]
    pub native_thinking: bool,
    #[serde(default)]
    pub budget_tokens: HashMap<String, u32>,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self {
            default_level: ThinkingLevel::Medium,
            native_thinking: false,
            budget_tokens: HashMap::new(),
        }
    }
}

impl ThinkingConfig {
    pub fn budget_tokens_for(&self, level: ThinkingLevel) -> Option<u32> {
        // Guard: only levels that have a built-in budget can use native thinking.
        let default = level.default_budget_tokens()?;
        Some(
            self.budget_tokens
                .get(level.as_str())
                .copied()
                .unwrap_or(default),
        )
    }

    pub fn warn_unknown_budget_keys(&self) {
        use ThinkingLevel::{High, Low, Max, Medium, Minimal, Off};
        const ALL_LEVELS: &[ThinkingLevel] = &[Off, Minimal, Low, Medium, High, Max];
        for key in self.budget_tokens.keys() {
            if !ALL_LEVELS.iter().any(|l| l.as_str() == key) {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"key": key})),
                    "Unknown thinking level in budget_tokens config; \
                     valid levels are: off, minimal, low, medium, high, max"
                );
            }
        }
    }
}

fn default_max_tokens() -> usize {
    8192
}
fn default_keep_recent() -> usize {
    4
}
fn default_collapse() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "agent.history_pruning"]
pub struct HistoryPrunerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_keep_recent")]
    pub keep_recent: usize,
    #[serde(default = "default_collapse")]
    pub collapse_tool_results: bool,
}

impl Default for HistoryPrunerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tokens: 8192,
            keep_recent: 4,
            collapse_tool_results: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "runtime_profile.auto_classify"]
pub struct AutoClassifyConfig {
    #[serde(default)]
    pub simple_hint: Option<String>,
    #[serde(default)]
    pub standard_hint: Option<String>,
    #[serde(default)]
    pub complex_hint: Option<String>,
}

fn default_min_quality_score() -> f64 {
    0.5
}
fn default_eval_max_retries() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "agent.eval"]
pub struct EvalConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_min_quality_score")]
    pub min_quality_score: f64,
    #[serde(default = "default_eval_max_retries")]
    pub max_retries: u32,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_quality_score: default_min_quality_score(),
            max_retries: default_eval_max_retries(),
        }
    }
}

fn default_cc_enabled() -> bool {
    false
}
fn default_threshold_ratio() -> f64 {
    0.50
}
fn default_protect_first_n() -> usize {
    3
}
fn default_protect_last_n() -> usize {
    4
}
fn default_cc_max_passes() -> u32 {
    3
}
fn default_summary_max_chars() -> usize {
    4000
}
fn default_source_max_chars() -> usize {
    50_000
}
fn default_cc_timeout_secs() -> u64 {
    60
}
fn default_identifier_policy() -> String {
    "strict".to_string()
}
fn default_tool_result_retrim_chars() -> usize {
    2_000
}

#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "agent.context_compression"]
pub struct ContextCompressionConfig {
    /// The runtime context compressor was removed; no runtime execution path
    /// consumes this flag, so setting it to `true` currently has no effect.
    /// Defaults to `false` to match actual runtime behavior;
    /// `Config::collect_warnings` reads an explicit `true` only to report
    /// `context_compression_unsupported`.
    #[serde(default = "default_cc_enabled")]
    pub enabled: bool,
    #[serde(default = "default_threshold_ratio")]
    pub threshold_ratio: f64,
    #[serde(default = "default_protect_first_n")]
    pub protect_first_n: usize,
    #[serde(default = "default_protect_last_n")]
    pub protect_last_n: usize,
    #[serde(default = "default_cc_max_passes")]
    pub max_passes: u32,
    #[serde(default = "default_summary_max_chars")]
    pub summary_max_chars: usize,
    #[serde(default = "default_source_max_chars")]
    pub source_max_chars: usize,
    #[serde(default = "default_cc_timeout_secs")]
    pub timeout_secs: u64,
    /// Summarizer provider as a `<type>.<alias>` reference into `providers.models`.
    #[serde(default)]
    pub summary_provider: crate::providers::ModelProviderRef,
    /// DEPRECATED bare model id retained as a compatibility fallback.
    #[serde(default)]
    pub summary_model: Option<String>,
    #[serde(default = "default_identifier_policy")]
    pub identifier_policy: String,
    #[serde(default = "default_tool_result_retrim_chars")]
    pub tool_result_retrim_chars: usize,
    #[serde(default)]
    pub tool_result_trim_exempt: Vec<String>,
}

impl Default for ContextCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: default_cc_enabled(),
            threshold_ratio: default_threshold_ratio(),
            protect_first_n: default_protect_first_n(),
            protect_last_n: default_protect_last_n(),
            max_passes: default_cc_max_passes(),
            summary_max_chars: default_summary_max_chars(),
            source_max_chars: default_source_max_chars(),
            timeout_secs: default_cc_timeout_secs(),
            summary_provider: crate::providers::ModelProviderRef::default(),
            summary_model: None,
            identifier_policy: default_identifier_policy(),
            tool_result_retrim_chars: default_tool_result_retrim_chars(),
            tool_result_trim_exempt: Vec::new(),
        }
    }
}

fn default_precheck_enabled() -> bool {
    true
}
fn default_precheck_timeout_secs() -> u64 {
    5
}

/// Per-channel reply-intent precheck configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "agent.precheck"]
pub struct ChannelPrecheckConfig {
    /// When false, the precheck is skipped entirely for this agent and every
    /// accepted channel message triggers the full agent loop. Default: `true`.
    #[serde(default = "default_precheck_enabled")]
    pub enabled: bool,
    /// Hard ceiling (seconds) on the precheck LLM call. On timeout the
    /// precheck fails open to REPLY. Default: `5`.
    #[serde(default = "default_precheck_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for ChannelPrecheckConfig {
    fn default() -> Self {
        Self {
            enabled: default_precheck_enabled(),
            timeout_secs: default_precheck_timeout_secs(),
        }
    }
}

// ── Tools config types ──────────────────────────────────────────

// ── Trust config types ──────────────────────────────────────────

fn default_initial_score() -> f64 {
    0.8
}
fn default_decay_half_life() -> f64 {
    30.0
}
fn default_regression_threshold() -> f64 {
    0.5
}
fn default_correction_penalty() -> f64 {
    0.05
}
fn default_success_boost() -> f64 {
    0.01
}

#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "trust"]
pub struct TrustConfig {
    #[serde(default = "default_initial_score")]
    pub initial_score: f64,
    #[serde(default = "default_decay_half_life")]
    pub decay_half_life_days: f64,
    #[serde(default = "default_regression_threshold")]
    pub regression_threshold: f64,
    #[serde(default = "default_correction_penalty")]
    pub correction_penalty: f64,
    #[serde(default = "default_success_boost")]
    pub success_boost: f64,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            initial_score: default_initial_score(),
            decay_half_life_days: default_decay_half_life(),
            regression_threshold: default_regression_threshold(),
            correction_penalty: default_correction_penalty(),
            success_boost: default_success_boost(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_level_from_str_canonical_aliases() {
        let cases = [
            ("off", ThinkingLevel::Off),
            ("none", ThinkingLevel::Off),
            ("minimal", ThinkingLevel::Minimal),
            ("min", ThinkingLevel::Minimal),
            ("low", ThinkingLevel::Low),
            ("medium", ThinkingLevel::Medium),
            ("med", ThinkingLevel::Medium),
            ("default", ThinkingLevel::Medium),
            ("high", ThinkingLevel::High),
            ("max", ThinkingLevel::Max),
            ("maximum", ThinkingLevel::Max),
        ];
        for (s, expected) in cases {
            assert_eq!(
                ThinkingLevel::from_str_insensitive(s),
                Some(expected),
                "input={s}"
            );
        }
    }

    #[test]
    fn thinking_level_from_str_case_insensitive() {
        assert_eq!(
            ThinkingLevel::from_str_insensitive("OFF"),
            Some(ThinkingLevel::Off)
        );
        assert_eq!(
            ThinkingLevel::from_str_insensitive("HIGH"),
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            ThinkingLevel::from_str_insensitive("MAX"),
            Some(ThinkingLevel::Max)
        );
        assert_eq!(
            ThinkingLevel::from_str_insensitive("Medium"),
            Some(ThinkingLevel::Medium)
        );
    }

    #[test]
    fn thinking_level_from_str_unknown_returns_none() {
        assert_eq!(ThinkingLevel::from_str_insensitive("unknown"), None);
        assert_eq!(ThinkingLevel::from_str_insensitive(""), None);
        assert_eq!(ThinkingLevel::from_str_insensitive("ultra"), None);
        assert_eq!(ThinkingLevel::from_str_insensitive("123"), None);
    }

    #[test]
    fn thinking_level_as_str_roundtrips() {
        for level in [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ] {
            let s = level.as_str();
            assert_eq!(
                ThinkingLevel::from_str_insensitive(s),
                Some(level),
                "roundtrip failed for {s}"
            );
        }
    }

    #[test]
    fn thinking_level_budget_tokens_only_high_and_max() {
        assert_eq!(ThinkingLevel::Off.default_budget_tokens(), None);
        assert_eq!(ThinkingLevel::Minimal.default_budget_tokens(), None);
        assert_eq!(ThinkingLevel::Low.default_budget_tokens(), None);
        assert_eq!(ThinkingLevel::Medium.default_budget_tokens(), None);
        assert_eq!(ThinkingLevel::High.default_budget_tokens(), Some(10_000));
        assert_eq!(ThinkingLevel::Max.default_budget_tokens(), Some(50_000));
    }

    // The runtime context compressor was removed; nothing reads
    // `context_compression` at runtime anymore, so the default must be
    // `false` (a `true` default would mislead users into thinking the
    // knob does something). See `context_compression_unsupported` in
    // `schema.rs` for the companion validation warning.
    #[test]
    fn context_compression_config_defaults_to_disabled() {
        assert!(!ContextCompressionConfig::default().enabled);
    }
}
