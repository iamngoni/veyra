//! Configuration for the TypeSafe/Jev integration boundary.
//!
//! `VEYRA_JEV_API_KEY` enables the integration; `VEYRA_JEV_PROVIDER`,
//! `VEYRA_JEV_BASE_URL`, and `VEYRA_JEV_MODEL` refine it. An absent key with
//! no related variables disables Jev entirely, and a partial section fails
//! closed. The key is redacted from `Debug` output and never logged.

use std::fmt;
use std::time::Duration;

use crate::config::ConfigError;

use super::JevProvider;

/// API credential; `Debug` output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct JevApiKey(String);

impl JevApiKey {
    /// Parses a key of 16-256 characters from `[A-Za-z0-9_-]`.
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidEnvironmentVariable`] for short,
    /// overlong, or non-conforming keys.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let trimmed = value.trim();
        let valid = (16..=256).contains(&trimmed.len())
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        if valid {
            Ok(Self(trimmed.to_owned()))
        } else {
            Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_JEV_API_KEY",
                reason: "must be 16-256 characters of letters, digits, '-' or '_'",
            })
        }
    }

    /// Exposes the secret for transport configuration.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for JevApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JevApiKey(redacted)")
    }
}

/// TypeSafe connection settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JevSettings {
    provider: JevProvider,
    api_key: JevApiKey,
    base_url: String,
    model: String,
}

impl JevSettings {
    /// Reads settings from the process environment.
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
    /// # Errors
    /// Returns [`ConfigError`] when the provider is unsupported, the key is
    /// missing or malformed, or the base URL or model is invalid.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Option<Self>, ConfigError> {
        let provider_raw = optional(&mut source, "VEYRA_JEV_PROVIDER");
        let key_raw = optional(&mut source, "VEYRA_JEV_API_KEY");
        let base_raw = optional(&mut source, "VEYRA_JEV_BASE_URL");
        let model_raw = optional(&mut source, "VEYRA_JEV_MODEL");

        if key_raw.is_empty() {
            let any_other =
                !provider_raw.is_empty() || !base_raw.is_empty() || !model_raw.is_empty();
            if any_other {
                return Err(ConfigError::MissingEnvironmentVariable {
                    name: "VEYRA_JEV_API_KEY",
                });
            }
            return Ok(None);
        }

        let provider = if provider_raw.is_empty() {
            JevProvider::TypeSafe
        } else {
            JevProvider::parse(&provider_raw).ok_or(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_JEV_PROVIDER",
                reason: "unsupported provider; supported values: typesafe",
            })?
        };

        let api_key = JevApiKey::parse(&key_raw)?;

        let base_url = if base_raw.is_empty() {
            "https://api.typesafe.ai".to_owned()
        } else {
            base_url(&base_raw)?
        };

        let model = if model_raw.is_empty() {
            "jev-latest".to_owned()
        } else {
            model_name(&model_raw)?
        };

        Ok(Some(Self {
            provider,
            api_key,
            base_url,
            model,
        }))
    }

    /// Provider identifier for status output.
    pub fn provider(&self) -> JevProvider {
        self.provider
    }

    /// Credential used for the authorization header.
    pub fn api_key(&self) -> &JevApiKey {
        &self.api_key
    }

    /// Base URL without a trailing slash (for example `https://api.typesafe.ai`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Model alias sent with every request.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Connect timeout for the shared HTTP client.
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }

    /// Total request timeout for the shared HTTP client; Jev answers in about
    /// a second, so this bounds a hung connection well above observed latency.
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(20)
    }
}

fn optional(
    source: &mut impl FnMut(&'static str) -> Result<String, ConfigError>,
    name: &'static str,
) -> String {
    source(name)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

fn base_url(value: &str) -> Result<String, ConfigError> {
    let invalid = |reason: &'static str| ConfigError::InvalidEnvironmentVariable {
        name: "VEYRA_JEV_BASE_URL",
        reason,
    };
    let (scheme, rest) = value
        .split_once("://")
        .ok_or_else(|| invalid("must be an absolute http(s) URL"))?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains(' ') {
        return Err(invalid("must include a host"));
    }
    let host = rest.split('/').next().unwrap_or_default();
    let loopback = host.starts_with("127.0.0.1")
        || host == "localhost"
        || host.starts_with("localhost:")
        || host.starts_with("[::1]");
    let allowed = scheme == "https" || (scheme == "http" && loopback);
    if !allowed {
        return Err(invalid(
            "must use https; http is accepted only on loopback for tests",
        ));
    }
    Ok(format!("{scheme}://{rest}"))
}

fn model_name(value: &str) -> Result<String, ConfigError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
    if valid {
        Ok(value.to_owned())
    } else {
        Err(ConfigError::InvalidEnvironmentVariable {
            name: "VEYRA_JEV_MODEL",
            reason: "must be 1-64 characters of letters, digits, '.', '-', '_' or ':'",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "apikey_test_1234567890";

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

    #[test]
    fn absent_section_disables_jev() {
        assert_eq!(JevSettings::from_source(source(&[])).expect("ok"), None);
    }

    #[test]
    fn partial_section_without_a_key_is_rejected() {
        let error = JevSettings::from_source(source(&[("VEYRA_JEV_MODEL", "jev-latest")]))
            .expect_err("partial settings must fail");
        assert!(matches!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_JEV_API_KEY"
            }
        ));
    }

    #[test]
    fn defaults_are_applied_and_overridable() {
        let settings = JevSettings::from_source(source(&[("VEYRA_JEV_API_KEY", KEY)]))
            .expect("valid")
            .expect("configured");
        assert_eq!(settings.provider(), JevProvider::TypeSafe);
        assert_eq!(settings.base_url(), "https://api.typesafe.ai");
        assert_eq!(settings.model(), "jev-latest");
        assert_eq!(settings.connect_timeout(), Duration::from_secs(5));
        assert_eq!(settings.request_timeout(), Duration::from_secs(20));

        let custom = JevSettings::from_source(source(&[
            ("VEYRA_JEV_API_KEY", KEY),
            ("VEYRA_JEV_PROVIDER", "typesafe"),
            ("VEYRA_JEV_BASE_URL", "http://127.0.0.1:8080/"),
            ("VEYRA_JEV_MODEL", "jev-1.13.0"),
        ]))
        .expect("valid")
        .expect("configured");
        assert_eq!(custom.base_url(), "http://127.0.0.1:8080");
        assert_eq!(custom.model(), "jev-1.13.0");
    }

    #[test]
    fn malformed_settings_are_rejected_by_name() {
        let cases: [(&'static str, &'static str); 6] = [
            ("VEYRA_JEV_PROVIDER", "openai"),
            ("VEYRA_JEV_API_KEY", "short"),
            ("VEYRA_JEV_API_KEY", "has spaces in it 1234567890"),
            ("VEYRA_JEV_BASE_URL", "api.typesafe.ai"),
            ("VEYRA_JEV_BASE_URL", "http://api.typesafe.ai"),
            ("VEYRA_JEV_MODEL", "bad model name"),
        ];
        for (name, value) in cases {
            // Put the malformed entry first so the injected source returns it
            // even when the case targets the API key itself.
            let pairs: [(&'static str, &'static str); 2] =
                [(name, value), ("VEYRA_JEV_API_KEY", KEY)];
            let error = JevSettings::from_source(source(&pairs)).expect_err("must be rejected");
            match error {
                ConfigError::InvalidEnvironmentVariable { name: reported, .. } => {
                    assert_eq!(reported, name, "{name}={value}")
                }
                other => panic!("unexpected error for {name}={value}: {other:?}"),
            }
        }

        let overlong = "x".repeat(257);
        let error = JevSettings::from_source(source(&[
            ("VEYRA_JEV_API_KEY", KEY),
            ("VEYRA_JEV_BASE_URL", &overlong),
        ]))
        .expect_err("oversized host must fail");
        assert!(matches!(
            error,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_JEV_BASE_URL",
                ..
            }
        ));
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let settings = JevSettings::from_source(source(&[("VEYRA_JEV_API_KEY", KEY)]))
            .expect("valid")
            .expect("configured");
        let debug = format!("{settings:?}");
        assert!(debug.contains("JevApiKey(redacted)"));
        assert!(!debug.contains(KEY));
        assert_eq!(format!("{:?}", settings.api_key()), "JevApiKey(redacted)");
        assert_eq!(settings.api_key().expose(), KEY);
    }
}
