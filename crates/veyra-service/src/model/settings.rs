//! Model configuration (`VEYRA_MODEL_*`).
//!
//! Secrets stay inside a redacted newtype, and parsing fails closed on partial
//! configuration so a half-configured provider never reaches request time.
//!
//! Tier models must accept a *forced tool call*: the structured path enforces
//! the response schema through `tool_choice`, and reasoning modes that reject
//! it fail with a provider error (for example DeepSeek "thinking mode does not
//! support this tool_choice"). Verified working on the OpenRouter account in
//! use: `openai/gpt-4o-mini`, `openai/gpt-4.1-mini`, `openai/gpt-4.1`,
//! `openai/gpt-5.4-mini`, `openai/gpt-5.4`.

use std::fmt;

use crate::config::ConfigError;
use crate::model::{ModelProvider, ModelTier};

/// Model API key that never appears in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wraps a non-empty key; blank or short values are configuration errors.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let trimmed = value.trim();
        if trimmed.len() < 8 {
            return Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MODEL_API_KEY",
                reason: "must be at least 8 characters",
            });
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Exposes the secret for transport configuration.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(redacted)")
    }
}

/// Concrete model per capability tier, each as an ordered candidate chain.
///
/// Index 0 is the configured primary; anything after it is a fallback tried in
/// order when the one before it cannot serve the request — the provider is out
/// of credits, rate limiting, overloaded, or rejects the model outright. A
/// tier always holds at least its primary, so an empty chain is unreachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierModels {
    fast: Vec<String>,
    balanced: Vec<String>,
    reasoning: Vec<String>,
}

impl TierModels {
    /// Builds tiers from validated model identifiers, with no fallbacks.
    pub fn new(
        fast: impl Into<String>,
        balanced: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        Self {
            fast: vec![fast.into()],
            balanced: vec![balanced.into()],
            reasoning: vec![reasoning.into()],
        }
    }

    /// Appends the fallback chain for one tier, preserving the primary at the
    /// head. Duplicates of an earlier candidate are dropped: retrying the same
    /// model against the same outage only burns budget.
    #[must_use]
    pub fn with_fallbacks(mut self, tier: ModelTier, fallbacks: Vec<String>) -> Self {
        let chain = match tier {
            ModelTier::Fast => &mut self.fast,
            ModelTier::Balanced => &mut self.balanced,
            ModelTier::Reasoning => &mut self.reasoning,
        };
        for candidate in fallbacks {
            if !chain.iter().any(|existing| existing == &candidate) {
                chain.push(candidate);
            }
        }
        self
    }

    /// Resolves a tier to its primary model identifier.
    pub fn resolve(&self, tier: ModelTier) -> &str {
        &self.chain(tier)[0]
    }

    /// The full ordered candidate chain for a tier: primary first.
    pub fn chain(&self, tier: ModelTier) -> &[String] {
        match tier {
            ModelTier::Fast => &self.fast,
            ModelTier::Balanced => &self.balanced,
            ModelTier::Reasoning => &self.reasoning,
        }
    }

    /// Fallbacks only, for status output and round-tripping configuration.
    pub fn fallbacks(&self, tier: ModelTier) -> &[String] {
        &self.chain(tier)[1..]
    }
}

/// Validated model settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSettings {
    provider: ModelProvider,
    api_key: ApiKey,
    base_url: Option<String>,
    tiers: TierModels,
    http_referer: Option<String>,
    app_title: Option<String>,
    app_hidden: bool,
    compel_structured_answer: bool,
    budget: crate::model::BudgetPolicy,
}

impl ModelSettings {
    /// Reads model settings from the process environment.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for partial or malformed settings.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_source(|name| {
            std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
        })
    }

    /// Parses an injected settings source.
    ///
    /// Absent key with no related variables disables model integration; any
    /// partially configured section fails closed.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when the provider is unsupported, the key is
    /// missing or short, a tier is missing, or the base URL is malformed.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Option<Self>, ConfigError> {
        let provider_raw = optional(&mut source, "VEYRA_MODEL_PROVIDER");
        let key_raw = optional(&mut source, "VEYRA_MODEL_API_KEY");
        let base_raw = optional(&mut source, "VEYRA_MODEL_BASE_URL");
        let fast_raw = optional(&mut source, "VEYRA_MODEL_FAST");
        let balanced_raw = optional(&mut source, "VEYRA_MODEL_BALANCED");
        let reasoning_raw = optional(&mut source, "VEYRA_MODEL_REASONING");
        let referer_raw = optional(&mut source, "VEYRA_MODEL_HTTP_REFERER");
        let title_raw = optional(&mut source, "VEYRA_MODEL_APP_TITLE");
        let hidden_raw = optional(&mut source, "VEYRA_MODEL_APP_HIDDEN");
        let compel_raw = optional(&mut source, "VEYRA_MODEL_COMPEL_STRUCTURED");
        let hourly_cap_raw = optional(&mut source, "VEYRA_MODEL_MAX_CALLS_PER_HOUR");
        let daily_cap_raw = optional(&mut source, "VEYRA_MODEL_MAX_CALLS_PER_DAY");
        let fallbacks_raw = optional(&mut source, "VEYRA_MODEL_FALLBACKS");
        let fast_fallbacks_raw = optional(&mut source, "VEYRA_MODEL_FAST_FALLBACKS");
        let balanced_fallbacks_raw = optional(&mut source, "VEYRA_MODEL_BALANCED_FALLBACKS");
        let reasoning_fallbacks_raw = optional(&mut source, "VEYRA_MODEL_REASONING_FALLBACKS");

        if key_raw.is_empty() {
            let any_other = !provider_raw.is_empty()
                || !base_raw.is_empty()
                || !fast_raw.is_empty()
                || !balanced_raw.is_empty()
                || !reasoning_raw.is_empty()
                || !referer_raw.is_empty()
                || !hourly_cap_raw.is_empty()
                || !daily_cap_raw.is_empty()
                || !fallbacks_raw.is_empty()
                || !fast_fallbacks_raw.is_empty()
                || !balanced_fallbacks_raw.is_empty()
                || !reasoning_fallbacks_raw.is_empty();
            if any_other {
                return Err(ConfigError::MissingEnvironmentVariable {
                    name: "VEYRA_MODEL_API_KEY",
                });
            }
            return Ok(None);
        }

        let provider = if provider_raw.is_empty() {
            ModelProvider::OpenRouter
        } else {
            ModelProvider::parse(&provider_raw).ok_or(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MODEL_PROVIDER",
                reason: "unsupported provider; supported values: openrouter",
            })?
        };

        let api_key = ApiKey::parse(&key_raw)?;

        let base_url = if base_raw.is_empty() {
            None
        } else if base_raw.starts_with("http://") || base_raw.starts_with("https://") {
            Some(base_raw)
        } else {
            return Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MODEL_BASE_URL",
                reason: "must start with http:// or https://",
            });
        };

        let tier = |name: &'static str, value: String| -> Result<String, ConfigError> {
            if value.is_empty() {
                Err(ConfigError::MissingEnvironmentVariable { name })
            } else {
                Ok(value)
            }
        };

        // A shared list applies to every tier; a tier-specific list replaces it
        // for that tier rather than extending it, so one narrow override never
        // has to restate the shared chain.
        let shared_fallbacks = parse_fallbacks("VEYRA_MODEL_FALLBACKS", &fallbacks_raw)?;
        let per_tier = |name: &'static str, raw: &str| -> Result<Vec<String>, ConfigError> {
            if raw.is_empty() {
                Ok(shared_fallbacks.clone())
            } else {
                parse_fallbacks(name, raw)
            }
        };

        let tiers = TierModels::new(
            tier("VEYRA_MODEL_FAST", fast_raw)?,
            tier("VEYRA_MODEL_BALANCED", balanced_raw)?,
            tier("VEYRA_MODEL_REASONING", reasoning_raw)?,
        )
        .with_fallbacks(
            ModelTier::Fast,
            per_tier("VEYRA_MODEL_FAST_FALLBACKS", &fast_fallbacks_raw)?,
        )
        .with_fallbacks(
            ModelTier::Balanced,
            per_tier("VEYRA_MODEL_BALANCED_FALLBACKS", &balanced_fallbacks_raw)?,
        )
        .with_fallbacks(
            ModelTier::Reasoning,
            per_tier("VEYRA_MODEL_REASONING_FALLBACKS", &reasoning_fallbacks_raw)?,
        );

        // Zero means unlimited; the cap bounds accidents, not normal use.
        let cap = |name: &'static str, raw: &str| -> Result<u32, ConfigError> {
            if raw.is_empty() {
                return Ok(0);
            }
            let invalid = || ConfigError::InvalidEnvironmentVariable {
                name,
                reason: "must be an integer from 0 through 100000 (0 = unlimited)",
            };
            let value = raw.parse::<u32>().map_err(|_| invalid())?;
            if value > 100_000 {
                return Err(invalid());
            }
            Ok(value)
        };
        let budget = crate::model::BudgetPolicy::new(
            cap("VEYRA_MODEL_MAX_CALLS_PER_HOUR", &hourly_cap_raw)?,
            cap("VEYRA_MODEL_MAX_CALLS_PER_DAY", &daily_cap_raw)?,
        );

        let http_referer = if referer_raw.is_empty() {
            None
        } else {
            Some(referer_raw)
        };
        let app_title = if title_raw.is_empty() {
            None
        } else {
            Some(title_raw)
        };
        // Unset means the provider's own default applies: the header is only
        // sent to opt out. Visibility is fixed on the first request the
        // provider ever sees, so diverging from that default silently would be
        // a decision nobody could reverse later.
        let app_hidden = match hidden_raw.as_str() {
            "" | "false" => false,
            "true" => true,
            _ => {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_MODEL_APP_HIDDEN",
                    reason: "must be `true` or `false`",
                });
            }
        };

        // Compelling the answer is the stronger guarantee and the right default
        // wherever a model accepts it. Reasoning models do not: they refuse the
        // request outright, so the whole loop fails rather than degrading.
        let compel_structured_answer = match compel_raw.as_str() {
            "" | "true" => true,
            "false" => false,
            _ => {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_MODEL_COMPEL_STRUCTURED",
                    reason: "must be `true` or `false`",
                });
            }
        };

        Ok(Some(Self {
            provider,
            api_key,
            base_url,
            tiers,
            http_referer,
            app_title,
            app_hidden,
            compel_structured_answer,
            budget,
        }))
    }

    /// Provider selected by configuration.
    pub fn provider(&self) -> ModelProvider {
        self.provider
    }

    /// Model API key.
    pub fn api_key(&self) -> &ApiKey {
        &self.api_key
    }

    /// Optional OpenAI-compatible base URL override.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    /// Configured model per tier.
    pub fn tiers(&self) -> &TierModels {
        &self.tiers
    }

    /// Optional OpenRouter attribution header.
    pub fn http_referer(&self) -> Option<&str> {
        self.http_referer.as_deref()
    }

    /// Display name reported alongside the attribution URL.
    pub fn app_title(&self) -> Option<&str> {
        self.app_title.as_deref()
    }

    /// Whether the attributed app asks to stay out of public rankings.
    pub fn app_hidden(&self) -> bool {
        self.app_hidden
    }

    /// Whether the provider is told it must return the schema, rather than
    /// being offered it as the obvious channel.
    ///
    /// True is stronger and is the default. Reasoning ("thinking") models
    /// reject a compelled choice with a 400, so they need this false.
    pub fn compel_structured_answer(&self) -> bool {
        self.compel_structured_answer
    }

    /// Call budget applied to the active engine.
    pub fn budget(&self) -> &crate::model::BudgetPolicy {
        &self.budget
    }
}

/// Reads an optional value; missing variables behave as empty strings.
fn optional(
    source: &mut impl FnMut(&'static str) -> Result<String, ConfigError>,
    name: &'static str,
) -> String {
    source(name)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// Maximum fallbacks accepted per tier.
///
/// Every candidate past the first costs a live round trip during an outage, so
/// the chain is bounded: a deep list turns one slow tick into a very slow one
/// and delays the decision loop far more than it rescues it.
const MAX_FALLBACKS_PER_TIER: usize = 4;

/// Parses a comma-separated fallback chain into validated model identifiers.
///
/// Blank segments are skipped so a trailing comma is harmless. Identifiers are
/// only shape-checked (`vendor/model`); whether the provider actually serves
/// one is not knowable here, and a wrong id simply fails over to the next
/// candidate at request time.
fn parse_fallbacks(name: &'static str, raw: &str) -> Result<Vec<String>, ConfigError> {
    let mut models = Vec::new();
    for segment in raw.split(',') {
        let candidate = segment.trim();
        if candidate.is_empty() {
            continue;
        }
        if !candidate.contains('/') || candidate.starts_with('/') || candidate.ends_with('/') {
            return Err(ConfigError::InvalidEnvironmentVariable {
                name,
                reason: "each fallback must be a `vendor/model` identifier",
            });
        }
        if models.iter().any(|existing| existing == candidate) {
            continue;
        }
        models.push(candidate.to_owned());
    }
    if models.len() > MAX_FALLBACKS_PER_TIER {
        return Err(ConfigError::InvalidEnvironmentVariable {
            name,
            reason: "at most 4 fallbacks per tier",
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-or-test-key-1234567890";

    fn source<'a>(
        pairs: &'a [(&'static str, &'a str)],
    ) -> impl FnMut(&'static str) -> Result<String, ConfigError> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(ConfigError::MissingEnvironmentVariable { name })
        }
    }

    /// Baseline settings with `overrides` replacing (not duplicating) entries,
    /// so tests can express "this one value is wrong" precisely.
    fn full<'a>(overrides: &'a [(&'static str, &'a str)]) -> Vec<(&'static str, &'a str)> {
        let mut pairs: Vec<(&'static str, &'a str)> = vec![
            ("VEYRA_MODEL_PROVIDER", "openrouter"),
            ("VEYRA_MODEL_API_KEY", KEY),
            ("VEYRA_MODEL_FAST", "vendor/fast"),
            ("VEYRA_MODEL_BALANCED", "vendor/balanced"),
            ("VEYRA_MODEL_REASONING", "vendor/reasoning"),
        ];
        for (key, value) in overrides {
            match pairs.iter_mut().find(|(existing, _)| existing == key) {
                Some(existing) => *existing = (key, value),
                None => pairs.push((key, value)),
            }
        }
        pairs
    }

    #[test]
    fn budget_caps_parse_and_fail_closed() {
        // Defaults are unlimited.
        let settings = ModelSettings::from_source(source(&full(&[])))
            .expect("settings parse")
            .expect("configured");
        assert_eq!(settings.budget().hourly(), 0);
        assert_eq!(settings.budget().daily(), 0);

        let settings = ModelSettings::from_source(source(&full(&[
            ("VEYRA_MODEL_MAX_CALLS_PER_HOUR", "120"),
            ("VEYRA_MODEL_MAX_CALLS_PER_DAY", "2000"),
        ])))
        .expect("settings parse")
        .expect("configured");
        assert_eq!(settings.budget().hourly(), 120);
        assert_eq!(settings.budget().daily(), 2000);

        for (name, value) in [
            ("VEYRA_MODEL_MAX_CALLS_PER_HOUR", "many"),
            ("VEYRA_MODEL_MAX_CALLS_PER_HOUR", "100001"),
            ("VEYRA_MODEL_MAX_CALLS_PER_DAY", "-1"),
        ] {
            let error = ModelSettings::from_source(source(&full(&[(name, value)])))
                .expect_err("malformed caps are rejected");
            assert!(
                matches!(
                    error,
                    ConfigError::InvalidEnvironmentVariable { name: rejected, .. } if rejected == name
                ),
                "unexpected error for {name}={value}: {error:?}"
            );
        }

        // A cap without a key is partial configuration and fails closed.
        let error = ModelSettings::from_source(source(&[("VEYRA_MODEL_MAX_CALLS_PER_HOUR", "10")]))
            .expect_err("caps without a key are partial");
        assert_eq!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_MODEL_API_KEY"
            }
        );
    }

    #[test]
    fn absent_section_disables_model_integration() {
        assert_eq!(ModelSettings::from_source(source(&[])).expect("ok"), None);
    }

    #[test]
    fn partial_sections_fail_closed() {
        // Provider only, no key.
        let error = ModelSettings::from_source(source(&[("VEYRA_MODEL_PROVIDER", "openrouter")]))
            .expect_err("must fail");
        assert!(matches!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_MODEL_API_KEY"
            }
        ));

        // Key without a tier.
        let error = ModelSettings::from_source(source(&[
            ("VEYRA_MODEL_API_KEY", KEY),
            ("VEYRA_MODEL_FAST", "vendor/fast"),
        ]))
        .expect_err("must fail");
        assert!(matches!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_MODEL_BALANCED"
            }
        ));
    }

    #[test]
    fn unknown_provider_and_bad_base_url_are_rejected() {
        let error = ModelSettings::from_source(source(&full(&[(
            "VEYRA_MODEL_PROVIDER",
            "not-a-provider",
        )])))
        .expect_err("must fail");
        assert!(matches!(
            error,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MODEL_PROVIDER",
                ..
            }
        ));

        let error =
            ModelSettings::from_source(source(&full(&[("VEYRA_MODEL_BASE_URL", "not-a-url")])))
                .expect_err("must fail");
        assert!(matches!(
            error,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MODEL_BASE_URL",
                ..
            }
        ));
    }

    #[test]
    fn short_keys_are_rejected_and_debug_is_redacted() {
        let error = ModelSettings::from_source(source(&full(&[("VEYRA_MODEL_API_KEY", "short")])))
            .expect_err("must fail");
        assert!(matches!(
            error,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MODEL_API_KEY",
                ..
            }
        ));

        let settings = ModelSettings::from_source(source(&full(&[(
            "VEYRA_MODEL_BASE_URL",
            "https://openrouter.ai/api/v1",
        )])))
        .expect("parse")
        .expect("configured");
        let debug = format!("{settings:?}");
        assert!(!debug.contains(KEY), "debug must not leak the key: {debug}");
        assert_eq!(debug.matches("ApiKey(redacted)").count(), 1);
        assert_eq!(settings.provider(), ModelProvider::OpenRouter);
        assert_eq!(settings.base_url(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(settings.http_referer(), None);
        assert_eq!(
            settings.tiers().resolve(ModelTier::Reasoning),
            "vendor/reasoning"
        );
        assert_eq!(format!("{}", ModelProvider::OpenRouter), "openrouter");
    }

    #[test]
    fn provider_defaults_to_openrouter_when_unset() {
        let pairs: Vec<(&'static str, &str)> = full(&[])
            .into_iter()
            .filter(|(key, _)| *key != "VEYRA_MODEL_PROVIDER")
            .collect();
        let settings = ModelSettings::from_source(source(&pairs))
            .expect("parse")
            .expect("configured");
        assert_eq!(settings.provider(), ModelProvider::OpenRouter);
        assert_eq!(
            ModelProvider::parse(" OpenRouter "),
            Some(ModelProvider::OpenRouter)
        );
        assert_eq!(ModelProvider::parse("jaeger"), None);
    }

    #[test]
    fn a_shared_fallback_list_applies_to_every_tier() {
        let settings = ModelSettings::from_source(source(&full(&[(
            "VEYRA_MODEL_FALLBACKS",
            "z-ai/glm-5.3-flash, xiaomi/mimo-v2.6-flash",
        )])))
        .expect("settings parse")
        .expect("configured");

        for tier in [ModelTier::Fast, ModelTier::Balanced, ModelTier::Reasoning] {
            assert_eq!(
                settings.tiers().fallbacks(tier),
                ["z-ai/glm-5.3-flash", "xiaomi/mimo-v2.6-flash"],
                "tier {tier:?} inherits the shared chain"
            );
        }
        assert_eq!(
            settings.tiers().chain(ModelTier::Balanced).first().unwrap(),
            "vendor/balanced",
            "the primary stays at the head of the chain"
        );
    }

    #[test]
    fn a_tier_specific_list_replaces_the_shared_one() {
        let settings = ModelSettings::from_source(source(&full(&[
            ("VEYRA_MODEL_FALLBACKS", "z-ai/glm-5.3-flash"),
            ("VEYRA_MODEL_REASONING_FALLBACKS", "xiaomi/mimo-v2.6-pro"),
        ])))
        .expect("settings parse")
        .expect("configured");

        assert_eq!(
            settings.tiers().fallbacks(ModelTier::Reasoning),
            ["xiaomi/mimo-v2.6-pro"],
            "the narrow override wins outright rather than extending"
        );
        assert_eq!(
            settings.tiers().fallbacks(ModelTier::Fast),
            ["z-ai/glm-5.3-flash"],
            "untouched tiers keep the shared chain"
        );
    }

    #[test]
    fn a_fallback_repeating_the_primary_is_dropped() {
        let settings = ModelSettings::from_source(source(&full(&[(
            "VEYRA_MODEL_BALANCED_FALLBACKS",
            "vendor/balanced, z-ai/glm-5.3-flash",
        )])))
        .expect("settings parse")
        .expect("configured");

        assert_eq!(
            settings.tiers().chain(ModelTier::Balanced),
            ["vendor/balanced", "z-ai/glm-5.3-flash"],
            "retrying the primary against the same outage only burns budget"
        );
    }

    #[test]
    fn malformed_and_oversized_fallback_lists_fail_closed() {
        let bad = ModelSettings::from_source(source(&full(&[(
            "VEYRA_MODEL_FALLBACKS",
            "not-a-model-id",
        )])));
        assert!(
            matches!(
                bad,
                Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_MODEL_FALLBACKS",
                    ..
                })
            ),
            "an identifier without a vendor is a configuration error"
        );

        let too_many = ModelSettings::from_source(source(&full(&[(
            "VEYRA_MODEL_FALLBACKS",
            "a/1, b/2, c/3, d/4, e/5",
        )])));
        assert!(
            matches!(
                too_many,
                Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_MODEL_FALLBACKS",
                    ..
                })
            ),
            "a deep chain turns one slow tick into a very slow one"
        );

        // A trailing comma is a typo, not a failure.
        let forgiving = ModelSettings::from_source(source(&full(&[(
            "VEYRA_MODEL_FALLBACKS",
            "z-ai/glm-5.3-flash,",
        )])))
        .expect("settings parse")
        .expect("configured");
        assert_eq!(
            forgiving.tiers().fallbacks(ModelTier::Fast),
            ["z-ai/glm-5.3-flash"]
        );
    }

    #[test]
    fn fallbacks_alone_still_require_a_key() {
        let orphaned =
            ModelSettings::from_source(source(&[("VEYRA_MODEL_FALLBACKS", "z-ai/glm-5.3-flash")]));
        assert!(matches!(
            orphaned,
            Err(ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_MODEL_API_KEY"
            })
        ));
    }
}
