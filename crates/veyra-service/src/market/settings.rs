//! Configuration for market-data integrations.
//!
//! `VEYRA_MARKET_PROVIDER` selects the implementation. Absent provider plus no
//! related variables disables market data entirely; partial or malformed
//! configuration fails startup.

use std::time::Duration;

use crate::config::ConfigError;
use crate::market::MarketProvider;

/// Default window to wait for a rates command acknowledgement.
const DEFAULT_AWAIT_SECS: u64 = 20;
/// Smallest await window that can outlive the command timeout.
const MIN_AWAIT_SECS: u64 = 5;
/// Largest await window the parser accepts.
const MAX_AWAIT_SECS: u64 = 120;

/// EA-backed market settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EaMarketSettings {
    await_timeout: Duration,
}

impl EaMarketSettings {
    /// Builds settings from an already validated await window.
    pub fn new(await_timeout: Duration) -> Self {
        Self { await_timeout }
    }

    /// How long one `rates` round trip may take before it is reported
    /// unavailable. Must exceed the broker command timeout (15 s) to be
    /// useful.
    pub fn await_timeout(&self) -> Duration {
        self.await_timeout
    }
}

/// Market-data configuration selected by `VEYRA_MARKET_PROVIDER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketSettings {
    /// MetaTrader 4 terminal, through the EA control channel.
    Ea(EaMarketSettings),
}

impl MarketSettings {
    /// Reads market settings from the process environment.
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
        let provider_raw = optional(&mut source, "VEYRA_MARKET_PROVIDER");
        let await_raw = optional(&mut source, "VEYRA_MARKET_EA_AWAIT_SECS");

        if provider_raw.is_empty() {
            if await_raw.is_empty() {
                return Ok(None);
            }
            return Err(ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_MARKET_PROVIDER",
            });
        }

        let provider = MarketProvider::parse(&provider_raw).ok_or(
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MARKET_PROVIDER",
                reason: "unsupported provider; supported values: ea",
            },
        )?;

        match provider {
            MarketProvider::Ea => {
                let await_timeout = match await_raw.as_str() {
                    "" => Duration::from_secs(DEFAULT_AWAIT_SECS),
                    other => {
                        let invalid = || ConfigError::InvalidEnvironmentVariable {
                            name: "VEYRA_MARKET_EA_AWAIT_SECS",
                            reason: "must be an integer number of seconds from 5 through 120",
                        };
                        let secs = other.parse::<u64>().map_err(|_| invalid())?;
                        if !(MIN_AWAIT_SECS..=MAX_AWAIT_SECS).contains(&secs) {
                            return Err(invalid());
                        }
                        Duration::from_secs(secs)
                    }
                };
                Ok(Some(Self::Ea(EaMarketSettings { await_timeout })))
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_provider_disables_market_data() {
        let settings = MarketSettings::from_source(|name| {
            Err(ConfigError::MissingEnvironmentVariable { name })
        })
        .expect("absent settings are fine");
        assert!(settings.is_none());
    }

    #[test]
    fn provider_only_settings_use_the_default_await_window() {
        let settings = MarketSettings::from_source(|name| match name {
            "VEYRA_MARKET_PROVIDER" => Ok("ea".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        match settings {
            MarketSettings::Ea(ea) => assert_eq!(ea.await_timeout(), Duration::from_secs(20)),
        }
    }

    #[test]
    fn partial_and_malformed_settings_fail_closed() {
        let error = MarketSettings::from_source(|name| match name {
            "VEYRA_MARKET_EA_AWAIT_SECS" => Ok("30".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect_err("await window without a provider is partial configuration");
        assert_eq!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_MARKET_PROVIDER"
            }
        );

        let error = MarketSettings::from_source(|name| match name {
            "VEYRA_MARKET_PROVIDER" => Ok("forex-api".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect_err("unknown providers are rejected");
        assert!(matches!(
            error,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_MARKET_PROVIDER",
                ..
            }
        ));

        for value in ["0", "4", "121", "soon"] {
            let error = MarketSettings::from_source(|name| match name {
                "VEYRA_MARKET_PROVIDER" => Ok("ea".to_owned()),
                "VEYRA_MARKET_EA_AWAIT_SECS" => Ok(value.to_owned()),
                _ => Err(ConfigError::MissingEnvironmentVariable { name }),
            })
            .expect_err("out-of-range await windows are rejected");
            assert!(
                matches!(
                    error,
                    ConfigError::InvalidEnvironmentVariable {
                        name: "VEYRA_MARKET_EA_AWAIT_SECS",
                        ..
                    }
                ),
                "unexpected error for {value}: {error:?}"
            );
        }
    }

    #[test]
    fn await_window_is_configurable_within_bounds() {
        let settings = MarketSettings::from_source(|name| match name {
            "VEYRA_MARKET_PROVIDER" => Ok(" EA ".to_owned()),
            "VEYRA_MARKET_EA_AWAIT_SECS" => Ok(" 45 ".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        match settings {
            MarketSettings::Ea(ea) => assert_eq!(ea.await_timeout(), Duration::from_secs(45)),
        }
    }
}
