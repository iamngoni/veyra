//! Configuration for calendar integrations.
//!
//! `VEYRA_CALENDAR_PROVIDER` selects the implementation. Absent provider plus
//! no related variables leaves the calendar unconfigured; partial or malformed
//! configuration fails startup.

use std::time::Duration;

use crate::calendar::CalendarProvider;
use crate::config::ConfigError;

/// Default window to wait for one calendar HTTP request.
const DEFAULT_TIMEOUT_SECS: u64 = 5;
/// Smallest accepted request timeout.
const MIN_TIMEOUT_SECS: u64 = 1;
/// Largest accepted request timeout.
const MAX_TIMEOUT_SECS: u64 = 60;
/// Default lifetime of a fetched weekly calendar.
const DEFAULT_CACHE_SECS: u64 = 900;
/// Smallest accepted cache lifetime.
const MIN_CACHE_SECS: u64 = 60;
/// Largest accepted cache lifetime.
const MAX_CACHE_SECS: u64 = 86_400;

/// ForexFactory weekly-calendar settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForexfactorySettings {
    timeout: Duration,
    cache_ttl: Duration,
}

impl ForexfactorySettings {
    /// Builds settings from validated durations.
    pub fn new(timeout: Duration, cache_ttl: Duration) -> Self {
        Self { timeout, cache_ttl }
    }

    /// How long one calendar HTTP request may take.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// How long a fetched weekly calendar is reused before refetching.
    pub fn cache_ttl(&self) -> Duration {
        self.cache_ttl
    }
}

/// Calendar configuration selected by `VEYRA_CALENDAR_PROVIDER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarSettings {
    /// The ForexFactory weekly calendar export.
    Forexfactory(ForexfactorySettings),
}

impl CalendarSettings {
    /// Reads calendar settings from the process environment.
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
    /// Returns [`ConfigError`] when the provider is unsupported or a related
    /// value is malformed.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Option<Self>, ConfigError> {
        let provider_raw = optional(&mut source, "VEYRA_CALENDAR_PROVIDER");
        let timeout_raw = optional(&mut source, "VEYRA_CALENDAR_HTTP_TIMEOUT_SECS");
        let cache_raw = optional(&mut source, "VEYRA_CALENDAR_CACHE_SECS");

        if provider_raw.is_empty() {
            if timeout_raw.is_empty() && cache_raw.is_empty() {
                return Ok(None);
            }
            return Err(ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_CALENDAR_PROVIDER",
            });
        }

        let provider = CalendarProvider::parse(&provider_raw).ok_or(
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_CALENDAR_PROVIDER",
                reason: "unsupported provider; supported values: forexfactory",
            },
        )?;

        match provider {
            CalendarProvider::Forexfactory => {
                let timeout = parse_bounded(
                    &timeout_raw,
                    "VEYRA_CALENDAR_HTTP_TIMEOUT_SECS",
                    DEFAULT_TIMEOUT_SECS,
                    MIN_TIMEOUT_SECS,
                    MAX_TIMEOUT_SECS,
                )?;
                let cache_ttl = parse_bounded(
                    &cache_raw,
                    "VEYRA_CALENDAR_CACHE_SECS",
                    DEFAULT_CACHE_SECS,
                    MIN_CACHE_SECS,
                    MAX_CACHE_SECS,
                )?;
                Ok(Some(Self::Forexfactory(ForexfactorySettings::new(
                    Duration::from_secs(timeout),
                    Duration::from_secs(cache_ttl),
                ))))
            }
        }
    }
}

fn parse_bounded(
    raw: &str,
    name: &'static str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ConfigError> {
    if raw.is_empty() {
        return Ok(default);
    }
    let invalid = || ConfigError::InvalidEnvironmentVariable {
        name,
        reason: "must be an integer number of seconds inside the documented range",
    };
    let value = raw.parse::<u64>().map_err(|_| invalid())?;
    if !(min..=max).contains(&value) {
        return Err(invalid());
    }
    Ok(value)
}

fn optional(
    source: &mut impl FnMut(&'static str) -> Result<String, ConfigError>,
    name: &'static str,
) -> String {
    source(name)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_provider_disables_the_calendar() {
        let settings = CalendarSettings::from_source(|name| {
            Err(ConfigError::MissingEnvironmentVariable { name })
        })
        .expect("absent settings are fine");
        assert!(settings.is_none());
    }

    #[test]
    fn provider_only_settings_use_the_documented_defaults() {
        let settings = CalendarSettings::from_source(|name| match name {
            "VEYRA_CALENDAR_PROVIDER" => Ok("forexfactory".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        match settings {
            CalendarSettings::Forexfactory(feed) => {
                assert_eq!(feed.timeout(), Duration::from_secs(DEFAULT_TIMEOUT_SECS));
                assert_eq!(feed.cache_ttl(), Duration::from_secs(DEFAULT_CACHE_SECS));
            }
        }
    }

    #[test]
    fn partial_and_malformed_settings_fail_closed() {
        let error = CalendarSettings::from_source(|name| match name {
            "VEYRA_CALENDAR_HTTP_TIMEOUT_SECS" => Ok("5".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect_err("a timeout without a provider is partial configuration");
        assert_eq!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_CALENDAR_PROVIDER"
            }
        );

        let error = CalendarSettings::from_source(|name| match name {
            "VEYRA_CALENDAR_PROVIDER" => Ok("tradingview".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect_err("unknown providers are rejected");
        assert!(matches!(
            error,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_CALENDAR_PROVIDER",
                ..
            }
        ));

        for (name, value) in [
            ("VEYRA_CALENDAR_HTTP_TIMEOUT_SECS", "0"),
            ("VEYRA_CALENDAR_HTTP_TIMEOUT_SECS", "61"),
            ("VEYRA_CALENDAR_HTTP_TIMEOUT_SECS", "soon"),
            ("VEYRA_CALENDAR_CACHE_SECS", "30"),
            ("VEYRA_CALENDAR_CACHE_SECS", "86401"),
            ("VEYRA_CALENDAR_CACHE_SECS", "later"),
        ] {
            let error = CalendarSettings::from_source(|key| match key {
                "VEYRA_CALENDAR_PROVIDER" => Ok("forexfactory".to_owned()),
                other if other == name => Ok(value.to_owned()),
                other => Err(ConfigError::MissingEnvironmentVariable { name: other }),
            })
            .expect_err("out-of-range values are rejected");
            assert!(
                matches!(
                    error,
                    ConfigError::InvalidEnvironmentVariable {
                        name: failing,
                        ..
                    } if failing == name
                ),
                "unexpected error for {name}={value}: {error:?}"
            );
        }
    }

    #[test]
    fn bounded_values_are_configurable() {
        let settings = CalendarSettings::from_source(|name| match name {
            "VEYRA_CALENDAR_PROVIDER" => Ok(" ForexFactory ".to_owned()),
            "VEYRA_CALENDAR_HTTP_TIMEOUT_SECS" => Ok(" 12 ".to_owned()),
            "VEYRA_CALENDAR_CACHE_SECS" => Ok("120".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        match settings {
            CalendarSettings::Forexfactory(feed) => {
                assert_eq!(feed.timeout(), Duration::from_secs(12));
                assert_eq!(feed.cache_ttl(), Duration::from_secs(120));
            }
        }
    }
}
