use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zeroclaw_macros::Configurable;

use super::schema::{
    AnthropicModelProviderConfig, CustomModelProviderConfig, GeminiModelProviderConfig,
    GroqModelProviderConfig, LlamacppModelProviderConfig, LmstudioModelProviderConfig,
    MinimaxModelProviderConfig, ModelProviderConfig, OllamaModelProviderConfig,
    OpenAIModelProviderConfig, OsaurusModelProviderConfig, OvhModelProviderConfig,
    QwenModelProviderConfig,
};
use super::schema::{
    AssemblyAiTranscriptionProviderConfig, DeepgramTranscriptionProviderConfig,
    GoogleTranscriptionProviderConfig, GroqTranscriptionProviderConfig,
    LocalWhisperTranscriptionProviderConfig, OpenAiTranscriptionProviderConfig,
};
use super::schema::{
    EdgeTtsProviderConfig, ElevenLabsTtsProviderConfig, GoogleTtsProviderConfig,
    OpenAITtsProviderConfig, PiperTtsProviderConfig, TtsProviderConfig as TtsBaseConfig,
};

#[macro_export]
macro_rules! define_provider_ref {
    ($name:ident, $category_doc:literal $(, $validator:path)?) => {
        #[doc = concat!("Reference to a configured `[", $category_doc, ".<type>.<alias>]` entry.")]
        /// Empty value means "no preference" (opt-out). Non-empty values must
        /// resolve to a configured alias; `Config::validate()` enforces this.
        #[derive(
            Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize,
        )]
        #[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                $( $validator(&value).map_err(serde::de::Error::custom)?; )?
                Ok(Self(value))
            }
        }

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }

            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(v: String) -> Self {
                Self(v)
            }
        }

        impl From<&str> for $name {
            fn from(v: &str) -> Self {
                Self(v.to_string())
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> Self {
                v.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }
    };
}

define_provider_ref!(
    ModelProviderRef,
    "providers.models",
    reject_retired_model_provider
);
define_provider_ref!(TtsProviderRef, "providers.tts");
define_provider_ref!(TranscriptionProviderRef, "providers.transcription");
define_provider_ref!(ChannelRef, "channels");
define_provider_ref!(RiskProfileRef, "risk_profiles");
define_provider_ref!(RuntimeProfileRef, "runtime_profiles");
define_provider_ref!(PersonaRef, "personas");
define_provider_ref!(CardRef, "cards");

pub const MAX_FALLBACK_DEPTH: usize = 3;

/// A model-provider spelling that no longer has a schema slot or runtime.
///
/// URL-preset families collapse into `custom` with an explicit endpoint `uri`;
/// `replacement_base_urls` carries that family's former default endpoint (the
/// first entry) plus any regional alternatives. Legacy retirements that never
/// had a preset leave it empty.
#[derive(Debug, Clone, Copy)]
pub struct RetiredModelProvider {
    pub name: &'static str,
    pub replacement_base_urls: &'static [&'static str],
}

const fn retired_legacy(name: &'static str) -> RetiredModelProvider {
    RetiredModelProvider {
        name,
        replacement_base_urls: &[],
    }
}

const fn retired_compat(
    name: &'static str,
    replacement_base_urls: &'static [&'static str],
) -> RetiredModelProvider {
    RetiredModelProvider {
        name,
        replacement_base_urls,
    }
}

/// Compatibility guard only: these families no longer have a schema or runtime.
/// Keep the old spellings here so migrations and direct config deserialization
/// reject them instead of dropping credentials or selecting a different vendor.
pub const RETIRED_MODEL_PROVIDERS: &[RetiredModelProvider] = &[
    retired_legacy("bedrock"),
    retired_legacy("aws-bedrock"),
    retired_legacy("azure"),
    retired_legacy("azure_openai"),
    retired_legacy("azure-openai"),
    retired_legacy("telnyx"),
    retired_legacy("gemini_cli"),
    retired_legacy("gemini-cli"),
    retired_legacy("kilocli"),
    retired_legacy("kilo-cli"),
    retired_legacy("copilot"),
    retired_legacy("github-copilot"),
    retired_legacy("openrouter"),
    // URL-preset families retired in favour of `custom` + explicit endpoint `uri`.
    // Includes every alias spelling the old factory folded into the family.
    retired_compat("vercel", &["https://ai-gateway.vercel.sh/v1"]),
    retired_compat("vercel-ai", &["https://ai-gateway.vercel.sh/v1"]),
    retired_compat("cloudflare", &["https://gateway.ai.cloudflare.com/v1"]),
    retired_compat("cloudflare-ai", &["https://gateway.ai.cloudflare.com/v1"]),
    retired_compat("synthetic", &["https://api.synthetic.new/openai/v1"]),
    retired_compat("opencode", &["https://opencode.ai/zen/v1"]),
    retired_compat("opencode-zen", &["https://opencode.ai/zen/v1"]),
    retired_compat("opencode-go", &["https://opencode.ai/zen/v1"]),
    retired_compat("doubao", &["https://ark.cn-beijing.volces.com/api/v3"]),
    retired_compat("volcengine", &["https://ark.cn-beijing.volces.com/api/v3"]),
    retired_compat("ark", &["https://ark.cn-beijing.volces.com/api/v3"]),
    retired_compat("doubao-cn", &["https://ark.cn-beijing.volces.com/api/v3"]),
    retired_compat("mistral", &["https://api.mistral.ai/v1"]),
    retired_compat("deepseek", &["https://api.deepseek.com"]),
    retired_compat("together", &["https://api.together.xyz"]),
    retired_compat("together-ai", &["https://api.together.xyz"]),
    retired_compat("fireworks", &["https://api.fireworks.ai/inference/v1"]),
    retired_compat("fireworks-ai", &["https://api.fireworks.ai/inference/v1"]),
    retired_compat("novita", &["https://api.novita.ai/openai"]),
    retired_compat("perplexity", &["https://api.perplexity.ai"]),
    retired_compat("cohere", &["https://api.cohere.com/compatibility"]),
    retired_compat("sglang", &["http://localhost:30000/v1"]),
    retired_compat("vllm", &["http://localhost:8000/v1"]),
    retired_compat("astrai", &["https://as-trai.com/v1"]),
    retired_compat("siliconflow", &["https://api.siliconflow.com/v1"]),
    retired_compat("silicon-flow", &["https://api.siliconflow.com/v1"]),
    retired_compat("aihubmix", &["https://aihubmix.com/v1"]),
    retired_compat("litellm", &["http://localhost:4000/v1"]),
    retired_compat("lite-llm", &["http://localhost:4000/v1"]),
    retired_compat("cerebras", &["https://api.cerebras.ai/v1"]),
    retired_compat("sambanova", &["https://api.sambanova.ai/v1"]),
    retired_compat("hyperbolic", &["https://api.hyperbolic.xyz/v1"]),
    retired_compat("deepinfra", &["https://api.deepinfra.com/v1/openai"]),
    retired_compat("deep-infra", &["https://api.deepinfra.com/v1/openai"]),
    retired_compat("huggingface", &["https://router.huggingface.co/v1"]),
    retired_compat("hf", &["https://router.huggingface.co/v1"]),
    retired_compat("ai21", &["https://api.ai21.com/studio/v1"]),
    retired_compat("ai21-labs", &["https://api.ai21.com/studio/v1"]),
    retired_compat("reka", &["https://api.reka.ai/v1"]),
    retired_compat("baseten", &["https://inference.baseten.co/v1"]),
    retired_compat("nscale", &["https://inference.api.nscale.com/v1"]),
    retired_compat("anyscale", &["https://api.endpoints.anyscale.com/v1"]),
    retired_compat("nebius", &["https://api.tokenfactory.nebius.com/v1"]),
    retired_compat("friendli", &["https://api.friendli.ai/serverless/v1"]),
    retired_compat("friendliai", &["https://api.friendli.ai/serverless/v1"]),
    retired_compat("lepton", &["https://llama3-1-405b.lepton.run/api/v1"]),
    retired_compat("lepton-ai", &["https://llama3-1-405b.lepton.run/api/v1"]),
    retired_compat("manifest", &["https://app.manifest.build/v1"]),
    retired_compat("morph", &["https://api.morphllm.com/v1"]),
    retired_compat("github_models", &["https://models.github.ai/inference"]),
    retired_compat("github-models", &["https://models.github.ai/inference"]),
    retired_compat("upstage", &["https://api.upstage.ai/v1"]),
    retired_compat("featherless", &["https://api.featherless.ai/v1"]),
    retired_compat("arcee", &["https://api.arcee.ai/api/v1"]),
    retired_compat("lambda_ai", &["https://api.lambda.ai/v1"]),
    retired_compat("lambda-ai", &["https://api.lambda.ai/v1"]),
    retired_compat("inception", &["https://api.inceptionlabs.ai/v1"]),
    retired_compat(
        "stepfun",
        &["https://api.stepfun.ai/v1", "https://api.stepfun.com/v1"],
    ),
    retired_compat(
        "step",
        &["https://api.stepfun.ai/v1", "https://api.stepfun.com/v1"],
    ),
    retired_compat(
        "stepfun-intl",
        &["https://api.stepfun.ai/v1", "https://api.stepfun.com/v1"],
    ),
    retired_compat(
        "step-intl",
        &["https://api.stepfun.ai/v1", "https://api.stepfun.com/v1"],
    ),
    retired_compat("baichuan", &["https://api.baichuan-ai.com/v1"]),
    retired_compat("yi", &["https://api.lingyiwanwu.com/v1"]),
    retired_compat("01ai", &["https://api.lingyiwanwu.com/v1"]),
    retired_compat("lingyiwanwu", &["https://api.lingyiwanwu.com/v1"]),
    retired_compat("hunyuan", &["https://api.hunyuan.cloud.tencent.com/v1"]),
    retired_compat("tencent", &["https://api.hunyuan.cloud.tencent.com/v1"]),
    retired_compat("avian", &["https://api.avian.io/v1"]),
    retired_compat("deepmyst", &["https://api.deepmyst.com/v1"]),
    retired_compat("deep-myst", &["https://api.deepmyst.com/v1"]),
    retired_compat(
        "moonshot",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "kimi",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "kimi-cn",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "kimi-intl",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "kimi-global",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "kimi-code",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "kimi_coding",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "kimi_for_coding",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "moonshot-cn",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "moonshot-intl",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat(
        "moonshot-global",
        &[
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ],
    ),
    retired_compat("venice", &["https://api.venice.ai"]),
    retired_compat("nearai", &["https://cloud-api.near.ai/v1"]),
    retired_compat("atomic_chat", &["http://127.0.0.1:1337/v1"]),
    retired_compat("atomic-chat", &["http://127.0.0.1:1337/v1"]),
    retired_compat(
        "zai",
        &[
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ],
    ),
    retired_compat(
        "z.ai",
        &[
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ],
    ),
    retired_compat(
        "zai-global",
        &[
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ],
    ),
    retired_compat(
        "z.ai-global",
        &[
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ],
    ),
    retired_compat(
        "zai-cn",
        &[
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ],
    ),
    retired_compat(
        "z.ai-cn",
        &[
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ],
    ),
    retired_compat(
        "glm",
        &[
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ],
    ),
    retired_compat(
        "zhipu",
        &[
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ],
    ),
    retired_compat(
        "glm-global",
        &[
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ],
    ),
    retired_compat(
        "zhipu-global",
        &[
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ],
    ),
    retired_compat(
        "glm-cn",
        &[
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ],
    ),
    retired_compat(
        "zhipu-cn",
        &[
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ],
    ),
    retired_compat(
        "bigmodel",
        &[
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ],
    ),
    retired_compat("nvidia", &["https://integrate.api.nvidia.com/v1"]),
    retired_compat("nvidia-nim", &["https://integrate.api.nvidia.com/v1"]),
    retired_compat("build.nvidia.com", &["https://integrate.api.nvidia.com/v1"]),
    retired_compat("qianfan", &["https://qianfan.baidubce.com/v2"]),
    retired_compat("baidu", &["https://qianfan.baidubce.com/v2"]),
    retired_compat("kilo", &["https://api.kilo.ai/api/gateway"]),
    retired_compat("xai", &["https://api.x.ai/v1"]),
    retired_compat("grok", &["https://api.x.ai/v1"]),
];

/// A pre-write configuration refusal. Callers must not restore an earlier
/// snapshot after this error: the guard has not changed the on-disk file.
#[derive(Debug)]
pub struct RetiredModelProviderError(&'static RetiredModelProvider);

impl std::fmt::Display for RetiredModelProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let retired = self.0;
        if let Some((default_url, regional)) = retired.replacement_base_urls.split_first() {
            write!(
                formatter,
                "Model provider `{}` is retired. Use `providers.models.custom.<alias>` with \
                 `uri = \"{default_url}\"`",
                retired.name
            )?;
            if !regional.is_empty() {
                let regional = regional
                    .iter()
                    .map(|url| format!("\"{url}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, " (regional alternatives: {regional})")?;
            }
            write!(
                formatter,
                " instead; keys, endpoints and options are not migrated, so set them \
                 explicitly on the custom entry."
            )
        } else {
            write!(
                formatter,
                "Model provider `{}` is retired. Remove its configuration and explicitly \
                 configure a supported provider; credentials and endpoints are not migrated. \
                 The `gemini` API provider (including Gemini CLI OAuth credentials) remains \
                 supported.",
                retired.name
            )
        }
    }
}

impl std::error::Error for RetiredModelProviderError {}

/// Every retired spelling, for tests and exhaustive guards.
pub fn retired_model_provider_names() -> impl Iterator<Item = &'static str> {
    RETIRED_MODEL_PROVIDERS.iter().map(|retired| retired.name)
}

pub fn find_retired_model_provider(name: &str) -> Option<&'static RetiredModelProvider> {
    RETIRED_MODEL_PROVIDERS
        .iter()
        .find(|retired| retired.name == name)
}

pub fn reject_retired_model_provider(name: &str) -> anyhow::Result<()> {
    // References may include an alias or a legacy endpoint suffix. Never echo
    // either: endpoints and alias values can contain private information.
    // Whole-name match plus every separator-delimited prefix: spellings such
    // as `z.ai` contain the alias separator themselves.
    let candidates = std::iter::once(name).chain(
        name.match_indices(['.', ':'])
            .map(|(index, _)| &name[..index]),
    );
    for candidate in candidates {
        if let Some(retired) = find_retired_model_provider(candidate) {
            return Err(RetiredModelProviderError(retired).into());
        }
    }
    Ok(())
}

/// Inspect only provider selectors and family keys, never arbitrary string
/// values such as model IDs, credentials, headers, or operator-chosen aliases.
pub fn reject_retired_model_provider_config(value: &toml::Value) -> anyhow::Result<()> {
    fn check_selector(value: Option<&toml::Value>) -> anyhow::Result<()> {
        if let Some(name) = value.and_then(toml::Value::as_str) {
            reject_retired_model_provider(name)?;
        }
        Ok(())
    }
    for key in [
        "default_provider",
        "model_provider",
        "default_model_provider",
    ] {
        check_selector(value.get(key))?;
    }
    for map in [
        value.get("model_providers"),
        value.get("providers").and_then(|v| v.get("models")),
    ] {
        if let Some(table) = map.and_then(toml::Value::as_table) {
            for (family, entries) in table {
                reject_retired_model_provider(family)?;
                if let Some(aliases) = entries.as_table() {
                    for alias in aliases.values() {
                        if let Some(fallbacks) =
                            alias.get("fallback").and_then(toml::Value::as_array)
                        {
                            for fallback in fallbacks {
                                check_selector(Some(fallback))?;
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(providers) = value.get("providers") {
        check_selector(providers.get("default_provider"))?;
    }
    if let Some(agents) = value.get("agents").and_then(toml::Value::as_table) {
        for agent in agents.values() {
            check_selector(agent.get("provider"))?;
            check_selector(agent.get("model_provider"))?;
            check_selector(agent.get("classifier_provider"))?;
            check_selector(agent.get("summary_provider"))?;
        }
    }
    check_selector(
        value
            .get("multimodal")
            .and_then(|v| v.get("vision_model_provider")),
    )?;
    for routes in [
        value.get("model_routes"),
        value.get("providers").and_then(|v| v.get("model_routes")),
    ] {
        if let Some(routes) = routes.and_then(toml::Value::as_array) {
            for route in routes {
                check_selector(route.get("provider"))?;
                check_selector(route.get("model_provider"))?;
            }
        }
    }
    Ok(())
}

#[macro_export]
macro_rules! for_each_model_provider_slot {
    ($mac:ident) => {
        $mac! {
            (openai, "openai", OpenAIModelProviderConfig),
            (anthropic, "anthropic", AnthropicModelProviderConfig),
            (qwen, "qwen", QwenModelProviderConfig),
            (minimax, "minimax", MinimaxModelProviderConfig),
            (ollama, "ollama", OllamaModelProviderConfig),
            (gemini, "gemini", GeminiModelProviderConfig),
            (groq, "groq", GroqModelProviderConfig),
            (ovh, "ovh", OvhModelProviderConfig),
            (lmstudio, "lmstudio", LmstudioModelProviderConfig),
            (llamacpp, "llamacpp", LlamacppModelProviderConfig),
            (osaurus, "osaurus", OsaurusModelProviderConfig),
            (custom, "custom", CustomModelProviderConfig),
        }
    };
}

macro_rules! emit_model_providers_struct {
    ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
        /// Typed model provider container with one alias map per provider family.
        #[derive(Debug, Clone, Default, Serialize, Configurable)]
        #[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
        #[prefix = "providers.models"]
        pub struct ModelProviders {
            $(
                #[serde(default, skip_serializing_if = "HashMap::is_empty")]
                #[nested]
                pub $field: HashMap<String, $cfg_ty>,
            )+
        }

        impl<'de> Deserialize<'de> for ModelProviders {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                struct Slots {
                    $(
                        #[serde(default)]
                        $field: HashMap<String, $cfg_ty>,
                    )+
                    #[serde(flatten)]
                    unknown: HashMap<String, serde::de::IgnoredAny>,
                }
                let slots = Slots::deserialize(deserializer)?;
                for family in slots.unknown.keys() {
                    reject_retired_model_provider(family).map_err(serde::de::Error::custom)?;
                }
                Ok(Self { $( $field: slots.$field, )+ })
            }
        }
    };
}
for_each_model_provider_slot!(emit_model_providers_struct);

impl ModelProviders {
    pub fn iter_entries(&self) -> impl Iterator<Item = (&'static str, &str, &ModelProviderConfig)> {
        let mut out: Vec<(&'static str, &str, &ModelProviderConfig)> = Vec::new();
        macro_rules! emit_iter {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                $(
                    for (alias, cfg) in &self.$field {
                        out.push(($type_str, alias.as_str(), &cfg.base));
                    }
                )+
            };
        }
        for_each_model_provider_slot!(emit_iter);
        out.into_iter()
    }

    /// Iterate every entry mutably across every typed slot.
    pub fn iter_entries_mut(
        &mut self,
    ) -> impl Iterator<Item = (&'static str, &str, &mut ModelProviderConfig)> {
        let mut out: Vec<(&'static str, &str, &mut ModelProviderConfig)> = Vec::new();
        macro_rules! emit_iter_mut {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                $(
                    for (alias, cfg) in self.$field.iter_mut() {
                        out.push(($type_str, alias.as_str(), &mut cfg.base));
                    }
                )+
            };
        }
        for_each_model_provider_slot!(emit_iter_mut);
        out.into_iter()
    }

    /// Resolve the family-default endpoint URI for `<family>.<alias>`. Returns
    /// `None` when the family is single-endpoint, unknown, or the alias is
    /// missing. Dispatch is generated by `for_each_model_provider_slot!`, so
    /// adding a family without a `FamilyEndpoint` impl is a compile error.
    pub fn resolved_endpoint_uri(&self, family: &str, alias: &str) -> Option<&'static str> {
        use super::schema::FamilyEndpoint;
        macro_rules! emit_endpoint {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                match family {
                    $( $type_str => self.$field.get(alias).and_then(|cfg| cfg.endpoint_uri()), )+
                    _ => None,
                }
            };
        }
        for_each_model_provider_slot!(emit_endpoint)
    }

    /// Look up the shared base config for a given `<provider_type>.<alias>`
    /// pair. Returns `None` when the family isn't recognized OR when
    /// the alias doesn't exist in that family's typed slot.
    pub fn find(&self, family: &str, alias: &str) -> Option<&ModelProviderConfig> {
        macro_rules! emit_get {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                match family {
                    $( $type_str => self.$field.get(alias).map(|cfg| &cfg.base), )+
                    _ => None,
                }
            };
        }
        for_each_model_provider_slot!(emit_get)
    }

    pub fn find_by_name(&self, name: &str) -> Option<(&'static str, String, &ModelProviderConfig)> {
        if let Some((kind, alias)) = name.split_once('.') {
            macro_rules! emit_dotted {
                ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                    match kind {
                        $( $type_str => self.$field.get(alias).map(|c| ($type_str, alias.to_string(), &c.base)), )+
                        _ => None,
                    }
                };
            }
            return for_each_model_provider_slot!(emit_dotted);
        }
        let mut hit: Option<(&'static str, String, &ModelProviderConfig)> = None;
        macro_rules! emit_bare {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                $(
                    if let Some(c) = self.$field.get(name) {
                        if hit.is_some() {
                            return None; // ambiguous across kinds
                        }
                        hit = Some(($type_str, name.to_string(), &c.base));
                    }
                )+
            };
        }
        for_each_model_provider_slot!(emit_bare);
        hit
    }

    pub fn ensure(&mut self, family: &str, alias: &str) -> Option<&mut ModelProviderConfig> {
        macro_rules! emit_ensure {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                match family {
                    $(
                        $type_str => Some(
                            &mut self
                                .$field
                                .entry(alias.to_string())
                                .or_default()
                                .base,
                        ),
                    )+
                    _ => None,
                }
            };
        }
        for_each_model_provider_slot!(emit_ensure)
    }

    /// True when `family`'s typed slot has at least one configured
    /// alias entry. Returns `false` for unknown families.
    pub fn contains_model_provider_type(&self, family: &str) -> bool {
        macro_rules! emit_contains {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                match family {
                    $( $type_str => !self.$field.is_empty(), )+
                    _ => false,
                }
            };
        }
        for_each_model_provider_slot!(emit_contains)
    }

    /// Iterate the alias keys for a given model_provider type. Returns an empty
    /// iterator for unknown model_provider types.
    pub fn aliases_of<'a>(&'a self, family: &str) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        macro_rules! emit_aliases {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                match family {
                    $( $type_str => Box::new(self.$field.keys().map(String::as_str)), )+
                    _ => Box::new(std::iter::empty()),
                }
            };
        }
        for_each_model_provider_slot!(emit_aliases)
    }

    /// Canonical family slot names, straight from
    /// `for_each_model_provider_slot!`. Use this to distinguish "unknown
    /// family" from "known family, missing alias" in validation messages,
    /// and to detect raw-TOML sections that deserialization silently drops.
    #[must_use]
    pub fn slot_names() -> &'static [&'static str] {
        macro_rules! emit_slot_names {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                &[$($type_str),+]
            };
        }
        const NAMES: &[&str] = for_each_model_provider_slot!(emit_slot_names);
        NAMES
    }

    /// Remove the entry for `<provider_type>.<alias>`, returning whether it
    /// existed. Returns `false` for unknown families.
    pub fn remove_alias(&mut self, family: &str, alias: &str) -> bool {
        macro_rules! emit_remove {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                match family {
                    $( $type_str => self.$field.remove(alias).is_some(), )+
                    _ => false,
                }
            };
        }
        for_each_model_provider_slot!(emit_remove)
    }

    /// True when no slot has any entry.
    pub fn is_empty(&self) -> bool {
        macro_rules! emit_is_empty {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                $( self.$field.is_empty() && )+ true
            };
        }
        for_each_model_provider_slot!(emit_is_empty)
    }

    /// Total number of (provider_type, alias) entries across all slots.
    pub fn len(&self) -> usize {
        macro_rules! emit_len {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                0 $( + self.$field.len() )+
            };
        }
        for_each_model_provider_slot!(emit_len)
    }
}

/// Typed TTS-provider container — one slot per TTS family. Mirrors
/// `ModelProviders` but smaller (TTS has a closed set of 5 families:
/// openai, elevenlabs, google, edge, piper). No catch-all needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts"]
pub struct TtsProviders {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub openai: HashMap<String, OpenAITtsProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub elevenlabs: HashMap<String, ElevenLabsTtsProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub google: HashMap<String, GoogleTtsProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub edge: HashMap<String, EdgeTtsProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub piper: HashMap<String, PiperTtsProviderConfig>,
}

impl TtsProviders {
    /// Iterate every TTS entry across every typed slot, yielding
    /// `(family, alias, &base)` triples.
    pub fn iter_entries(
        &self,
    ) -> Box<dyn Iterator<Item = (&'static str, &str, &TtsBaseConfig)> + '_> {
        Box::new(
            std::iter::empty()
                .chain(
                    self.openai
                        .iter()
                        .map(|(a, c)| ("openai", a.as_str(), &c.base)),
                )
                .chain(
                    self.elevenlabs
                        .iter()
                        .map(|(a, c)| ("elevenlabs", a.as_str(), &c.base)),
                )
                .chain(
                    self.google
                        .iter()
                        .map(|(a, c)| ("google", a.as_str(), &c.base)),
                )
                .chain(self.edge.iter().map(|(a, c)| ("edge", a.as_str(), &c.base)))
                .chain(
                    self.piper
                        .iter()
                        .map(|(a, c)| ("piper", a.as_str(), &c.base)),
                ),
        )
    }

    /// Iterate every TTS entry mutably across every typed slot.
    pub fn iter_entries_mut(
        &mut self,
    ) -> Box<dyn Iterator<Item = (&'static str, &str, &mut TtsBaseConfig)> + '_> {
        Box::new(
            std::iter::empty()
                .chain(
                    self.openai
                        .iter_mut()
                        .map(|(a, c)| ("openai", a.as_str(), &mut c.base)),
                )
                .chain(
                    self.elevenlabs
                        .iter_mut()
                        .map(|(a, c)| ("elevenlabs", a.as_str(), &mut c.base)),
                )
                .chain(
                    self.google
                        .iter_mut()
                        .map(|(a, c)| ("google", a.as_str(), &mut c.base)),
                )
                .chain(
                    self.edge
                        .iter_mut()
                        .map(|(a, c)| ("edge", a.as_str(), &mut c.base)),
                )
                .chain(
                    self.piper
                        .iter_mut()
                        .map(|(a, c)| ("piper", a.as_str(), &mut c.base)),
                ),
        )
    }

    /// True when no slot has any entry.
    pub fn is_empty(&self) -> bool {
        self.openai.is_empty()
            && self.elevenlabs.is_empty()
            && self.google.is_empty()
            && self.edge.is_empty()
            && self.piper.is_empty()
    }
}

/// Typed transcription-provider container — one slot per STT family.
/// Mirrors `ModelProviders` / `TtsProviders`. Closed set of 6 families:
/// groq, openai, deepgram, assemblyai, google, local_whisper.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription"]
pub struct TranscriptionProviders {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub groq: HashMap<String, GroqTranscriptionProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub openai: HashMap<String, OpenAiTranscriptionProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub deepgram: HashMap<String, DeepgramTranscriptionProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub assemblyai: HashMap<String, AssemblyAiTranscriptionProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub google: HashMap<String, GoogleTranscriptionProviderConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub local_whisper: HashMap<String, LocalWhisperTranscriptionProviderConfig>,
}

pub enum TranscriptionProviderEntry<'a> {
    Groq(&'a GroqTranscriptionProviderConfig),
    OpenAi(&'a OpenAiTranscriptionProviderConfig),
    Deepgram(&'a DeepgramTranscriptionProviderConfig),
    AssemblyAi(&'a AssemblyAiTranscriptionProviderConfig),
    Google(&'a GoogleTranscriptionProviderConfig),
    LocalWhisper(&'a LocalWhisperTranscriptionProviderConfig),
}

impl TranscriptionProviders {
    /// True when no slot has any entry.
    pub fn is_empty(&self) -> bool {
        self.groq.is_empty()
            && self.openai.is_empty()
            && self.deepgram.is_empty()
            && self.assemblyai.is_empty()
            && self.google.is_empty()
            && self.local_whisper.is_empty()
    }

    /// Iterate every configured (family, alias) pair across all six slots.
    pub fn iter_aliases(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let mut out: Vec<(&'static str, &str)> = Vec::new();
        for k in self.groq.keys() {
            out.push(("groq", k.as_str()));
        }
        for k in self.openai.keys() {
            out.push(("openai", k.as_str()));
        }
        for k in self.deepgram.keys() {
            out.push(("deepgram", k.as_str()));
        }
        for k in self.assemblyai.keys() {
            out.push(("assemblyai", k.as_str()));
        }
        for k in self.google.keys() {
            out.push(("google", k.as_str()));
        }
        for k in self.local_whisper.keys() {
            out.push(("local_whisper", k.as_str()));
        }
        out.into_iter()
    }

    pub fn iter_entries(
        &self,
    ) -> Box<dyn Iterator<Item = (&'static str, &str, TranscriptionProviderEntry<'_>)> + '_> {
        Box::new(
            std::iter::empty()
                .chain(self.groq.iter().map(|(alias, config)| {
                    (
                        "groq",
                        alias.as_str(),
                        TranscriptionProviderEntry::Groq(config),
                    )
                }))
                .chain(self.openai.iter().map(|(alias, config)| {
                    (
                        "openai",
                        alias.as_str(),
                        TranscriptionProviderEntry::OpenAi(config),
                    )
                }))
                .chain(self.deepgram.iter().map(|(alias, config)| {
                    (
                        "deepgram",
                        alias.as_str(),
                        TranscriptionProviderEntry::Deepgram(config),
                    )
                }))
                .chain(self.assemblyai.iter().map(|(alias, config)| {
                    (
                        "assemblyai",
                        alias.as_str(),
                        TranscriptionProviderEntry::AssemblyAi(config),
                    )
                }))
                .chain(self.google.iter().map(|(alias, config)| {
                    (
                        "google",
                        alias.as_str(),
                        TranscriptionProviderEntry::Google(config),
                    )
                }))
                .chain(self.local_whisper.iter().map(|(alias, config)| {
                    (
                        "local_whisper",
                        alias.as_str(),
                        TranscriptionProviderEntry::LocalWhisper(config),
                    )
                })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_model_provider_slots() -> Vec<&'static str> {
        macro_rules! collect_slot_names {
            ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
                vec![$($type_str),+]
            };
        }
        for_each_model_provider_slot!(collect_slot_names)
    }

    #[test]
    fn retired_spellings_never_collide_with_a_live_slot() {
        let live = live_model_provider_slots();
        for name in retired_model_provider_names() {
            assert!(
                !live.contains(&name),
                "{name} is both retired and a live slot"
            );
        }
        for slot in live {
            assert!(
                reject_retired_model_provider(&format!("{slot}.some_alias")).is_ok(),
                "live slot {slot} must not be refused"
            );
        }
    }

    #[test]
    fn retired_compat_families_name_the_custom_replacement_without_echoing_the_alias() {
        let mut checked = 0;
        for retired in RETIRED_MODEL_PROVIDERS
            .iter()
            .filter(|retired| !retired.replacement_base_urls.is_empty())
        {
            for reference in [
                retired.name.to_string(),
                format!("{}.private_alias", retired.name),
            ] {
                let error = reject_retired_model_provider(&reference)
                    .expect_err("retired compat family must be refused")
                    .to_string();
                assert!(
                    error.contains(&format!("`{}` is retired", retired.name)),
                    "{reference}: {error}"
                );
                assert!(error.contains("providers.models.custom.<alias>"), "{error}");
                assert!(
                    error.contains(&format!("uri = \"{}\"", retired.replacement_base_urls[0])),
                    "{reference}: {error}"
                );
                assert!(!error.contains("private_alias"), "{error}");
            }
            checked += 1;
        }
        // 54 compat-preset families, xai, and every alias spelling they folded.
        assert!(checked >= 55, "only {checked} retired compat spellings");
    }

    #[test]
    fn multi_region_retirements_list_every_regional_endpoint() {
        let error = reject_retired_model_provider("moonshot.home")
            .unwrap_err()
            .to_string();
        for url in [
            "https://api.moonshot.ai/v1",
            "https://api.moonshot.cn/v1",
            "https://api.kimi.com/coding/v1",
        ] {
            assert!(error.contains(url), "{error}");
        }
    }

    #[test]
    fn spellings_that_contain_the_alias_separator_are_still_refused() {
        for name in ["z.ai", "z.ai-cn", "build.nvidia.com", "z.ai.private_alias"] {
            assert!(reject_retired_model_provider(name).is_err(), "{name}");
        }
    }

    #[test]
    fn config_scan_refuses_retired_compat_selectors_everywhere_a_reference_resolves() {
        for body in [
            "[providers.models.mistral.home]\napi_key = 'private-test-key'\n",
            "[agents.owner]\nmodel_provider = 'deepseek.home'\n",
            "[agents.owner]\nsummary_provider = 'xai.home'\n",
            "[providers.models.openai.home]\nfallback = ['groq_like', 'together.home']\n",
            "[[model_routes]]\nhint = 'fast'\nmodel_provider = 'cerebras.home'\nmodel = 'm'\n",
            "[multimodal]\nvision_model_provider = 'moonshot.home'\n",
        ] {
            let value: toml::Value = toml::from_str(body).unwrap();
            let error = reject_retired_model_provider_config(&value)
                .expect_err(body)
                .to_string();
            assert!(error.contains("is retired"), "{body}: {error}");
            assert!(error.contains("providers.models.custom.<alias>"), "{error}");
            assert!(!error.contains("private-test-key"), "{error}");
        }
    }

    #[test]
    fn transcription_iter_entries_walks_every_typed_slot() {
        let mut providers = TranscriptionProviders::default();
        providers
            .groq
            .insert("fast".into(), GroqTranscriptionProviderConfig::default());
        providers.openai.insert(
            "whisper".into(),
            OpenAiTranscriptionProviderConfig::default(),
        );
        providers.deepgram.insert(
            "nova".into(),
            DeepgramTranscriptionProviderConfig::default(),
        );
        providers.assemblyai.insert(
            "default".into(),
            AssemblyAiTranscriptionProviderConfig::default(),
        );
        providers.google.insert(
            "speech".into(),
            GoogleTranscriptionProviderConfig::default(),
        );
        providers.local_whisper.insert(
            "lan".into(),
            LocalWhisperTranscriptionProviderConfig::default(),
        );

        let entries: Vec<(&str, String, &str)> = providers
            .iter_entries()
            .map(|(family, alias, entry)| {
                let variant = match entry {
                    TranscriptionProviderEntry::Groq(_) => "groq",
                    TranscriptionProviderEntry::OpenAi(_) => "openai",
                    TranscriptionProviderEntry::Deepgram(_) => "deepgram",
                    TranscriptionProviderEntry::AssemblyAi(_) => "assemblyai",
                    TranscriptionProviderEntry::Google(_) => "google",
                    TranscriptionProviderEntry::LocalWhisper(_) => "local_whisper",
                };
                (family, alias.to_string(), variant)
            })
            .collect();

        assert_eq!(
            entries,
            vec![
                ("groq", "fast".into(), "groq"),
                ("openai", "whisper".into(), "openai"),
                ("deepgram", "nova".into(), "deepgram"),
                ("assemblyai", "default".into(), "assemblyai"),
                ("google", "speech".into(), "google"),
                ("local_whisper", "lan".into(), "local_whisper"),
            ]
        );
    }
}

/// Top-level wrapper for every configured provider category.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers"]
pub struct Providers {
    /// Model providers — `[providers.models.<type>.<alias>]`.
    #[serde(default)]
    #[nested]
    pub models: ModelProviders,

    /// Text-to-speech providers — `[providers.tts.<type>.<alias>]`.
    #[serde(default)]
    #[nested]
    pub tts: TtsProviders,

    /// Transcription / speech-to-text providers — `[providers.transcription.<type>.<alias>]`.
    #[serde(default)]
    #[nested]
    pub transcription: TranscriptionProviders,
}

macro_rules! emit_model_cost_rates_struct {
    ($(($field:ident, $type_str:literal, $cfg_ty:ty)),+ $(,)?) => {
        /// `[cost.rates.providers.models.<type>.<model>]` — token-cost rates
        /// per (provider type, model). One slot per provider type; each
        /// slot is a `HashMap<model_id, ModelCostRates>`. The slot list
        /// matches `ModelProviders` byte-for-byte (same source macro).
        #[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
        #[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
        #[prefix = "cost.rates.providers.models"]
        pub struct ModelCostRatesByProvider {
            $(
                #[serde(default, skip_serializing_if = "HashMap::is_empty")]
                #[nested]
                #[resource_key]
                pub $field: HashMap<String, super::schema::ModelCostRates>,
            )+
        }

        impl ModelCostRatesByProvider {
            /// Lookup rates by `(provider_type, model_id)`.
            #[must_use]
            pub fn get(
                &self,
                provider_type: &str,
                model_id: &str,
            ) -> Option<&super::schema::ModelCostRates> {
                match provider_type {
                    $(
                        $type_str => self.$field.get(model_id),
                    )+
                    _ => None,
                }
            }

            /// Iterate every priced model across every slot, yielding
            /// `(provider_type, model_id, &rates)` triples. Mirrors
            /// `ModelProviders::iter_entries` so callers can walk both
            /// the providers and the rate sheet with the same loop shape.
            pub fn iter_entries(
                &self,
            ) -> impl Iterator<Item = (&'static str, &str, &super::schema::ModelCostRates)> {
                let mut out: Vec<(&'static str, &str, &super::schema::ModelCostRates)> = Vec::new();
                $(
                    for (model_id, rates) in &self.$field {
                        out.push(($type_str, model_id.as_str(), rates));
                    }
                )+
                out.into_iter()
            }

            /// True when no slot has any priced model.
            pub fn is_empty(&self) -> bool {
                $(self.$field.is_empty())&&+
            }
        }
    };
}
for_each_model_provider_slot!(emit_model_cost_rates_struct);

/// Slot list for TTS providers. Single source of truth shared between
/// the typed-provider wrapper and the cost-rates wrapper — adding a TTS
/// family is a one-line edit here.
#[macro_export]
macro_rules! for_each_tts_provider_slot {
    ($mac:ident, $rate_ty:ty) => {
        $mac! {
            $rate_ty,
            (openai, "openai"),
            (elevenlabs, "elevenlabs"),
            (google, "google"),
            (edge, "edge"),
            (piper, "piper"),
        }
    };
}

/// Slot list for transcription providers.
#[macro_export]
macro_rules! for_each_transcription_provider_slot {
    ($mac:ident, $rate_ty:ty) => {
        $mac! {
            $rate_ty,
            (groq, "groq"),
            (openai, "openai"),
            (deepgram, "deepgram"),
            (assemblyai, "assemblyai"),
            (google, "google"),
            (local_whisper, "local_whisper"),
        }
    };
}

/// Collect the `$type_str` names out of a rate-typed slot macro
/// (`for_each_tts_provider_slot!` / `for_each_transcription_provider_slot!`).
/// The `$rate_ty` head is consumed and ignored; the macro exists so
/// `slot_names()` derives from the same source the structs are built from.
macro_rules! collect_rate_slot_names {
    ($rate_ty:ty, $(($field:ident, $type_str:literal)),+ $(,)?) => {
        &[$($type_str),+]
    };
}

impl TtsProviders {
    /// Canonical TTS family slot names, derived from
    /// `for_each_tts_provider_slot!`.
    #[must_use]
    pub fn slot_names() -> &'static [&'static str] {
        const NAMES: &[&str] = for_each_tts_provider_slot!(collect_rate_slot_names, ());
        NAMES
    }
}

impl TranscriptionProviders {
    /// Canonical transcription family slot names, derived from
    /// `for_each_transcription_provider_slot!`.
    #[must_use]
    pub fn slot_names() -> &'static [&'static str] {
        const NAMES: &[&str] = for_each_transcription_provider_slot!(collect_rate_slot_names, ());
        NAMES
    }
}

/// Emit a `<Family>CostRatesByProvider` struct from a slot list. Used
/// by both the TTS and transcription cost-rate wrappers — every field,
/// every dispatch arm, every iter row expands from one slot list. No
/// hand-typed `match "openai" => self.openai` tables anywhere.
macro_rules! emit_simple_cost_rates_struct {
    (
        $struct_name:ident,
        $rate_ty:ty,
        $prefix:literal,
        $resource_doc:literal,
        $(($field:ident, $type_str:literal)),+ $(,)?
    ) => {
        #[doc = concat!("`", $prefix, ".<type>.<", $resource_doc, ">`")]
        ///  — per-(provider type, resource) cost rates.
        #[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
        #[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
        #[prefix = $prefix]
        pub struct $struct_name {
            $(
                #[serde(default, skip_serializing_if = "HashMap::is_empty")]
                #[nested]
                #[resource_key]
                pub $field: HashMap<String, $rate_ty>,
            )+
        }

        impl $struct_name {
            /// Lookup rates by `(provider_type, resource_id)`.
            #[must_use]
            pub fn get(&self, provider_type: &str, resource_id: &str) -> Option<&$rate_ty> {
                match provider_type {
                    $($type_str => self.$field.get(resource_id),)+
                    _ => None,
                }
            }

            /// Iterate `(provider_type, resource_id, &rates)` across every
            /// slot.
            pub fn iter_entries(
                &self,
            ) -> impl Iterator<Item = (&'static str, &str, &$rate_ty)> {
                let mut out: Vec<(&'static str, &str, &$rate_ty)> = Vec::new();
                $(
                    for (resource_id, rates) in &self.$field {
                        out.push(($type_str, resource_id.as_str(), rates));
                    }
                )+
                out.into_iter()
            }

            /// True when no slot has any priced resource.
            pub fn is_empty(&self) -> bool {
                $(self.$field.is_empty())&&+
            }
        }
    };
}

macro_rules! emit_tts_cost_rates_struct {
    ($rate_ty:ty, $($slot:tt),+ $(,)?) => {
        emit_simple_cost_rates_struct! {
            TtsCostRatesByProvider,
            $rate_ty,
            "cost.rates.providers.tts",
            "voice",
            $($slot),+
        }
    };
}
for_each_tts_provider_slot!(emit_tts_cost_rates_struct, super::schema::TtsCostRates);

macro_rules! emit_transcription_cost_rates_struct {
    ($rate_ty:ty, $($slot:tt),+ $(,)?) => {
        emit_simple_cost_rates_struct! {
            TranscriptionCostRatesByProvider,
            $rate_ty,
            "cost.rates.providers.transcription",
            "model",
            $($slot),+
        }
    };
}
for_each_transcription_provider_slot!(
    emit_transcription_cost_rates_struct,
    super::schema::TranscriptionCostRates
);
