//! Configuration for broker integrations.
//!
//! `VEYRA_BROKER_PROVIDER` selects the implementation; each implementation
//! validates its own settings during startup and fails closed on partial or
//! malformed input. Absent provider plus no related variables disables broker
//! integration entirely.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use crate::broker::BrokerProvider;
use crate::config::{ConfigError, Port};

/// Shared secret the EA presents on every poll. Debug output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct EaToken(String);

impl EaToken {
    /// Parses a token of 16-128 characters from `[A-Za-z0-9_-]`.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let trimmed = value.trim();
        let valid = (16..=128).contains(&trimmed.len())
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        if valid {
            Ok(Self(trimmed.to_owned()))
        } else {
            Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_EA_TOKEN",
                reason: "must be 16-128 characters of letters, digits, '-' or '_'",
            })
        }
    }

    /// Exposes the secret for constant-time comparison or transmission.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EaToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EaToken(redacted)")
    }
}

/// EA control-channel settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EaSettings {
    token: EaToken,
    bind: SocketAddr,
}

impl EaSettings {
    /// Builds settings from an already validated token and bind address.
    pub fn new(token: EaToken, bind: SocketAddr) -> Self {
        Self { token, bind }
    }

    /// Token required from the EA on every poll.
    pub fn token(&self) -> &EaToken {
        &self.token
    }

    /// Address the control channel binds.
    pub fn bind(&self) -> SocketAddr {
        self.bind
    }

    /// State older than this window is reported as stale. The EA heartbeats
    /// every second, so ten seconds means ten missed beats.
    pub fn stale_after(&self) -> Duration {
        Duration::from_secs(10)
    }

    /// An unacknowledged command fails after this window.
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(15)
    }
}

/// Broker configuration selected by `VEYRA_BROKER_PROVIDER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerSettings {
    /// MetaTrader 4 EA control channel.
    Ea(EaSettings),
}

/// Deployment boundary for the EA listener.
///
/// Non-loopback binding is an explicit opt-in for an isolated container
/// network. The default preserves the host deployment's loopback-only safety
/// invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EaBindPolicy {
    LoopbackOnly,
    IsolatedNetwork,
}

impl EaBindPolicy {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.trim() {
            "" | "false" => Ok(Self::LoopbackOnly),
            "true" => Ok(Self::IsolatedNetwork),
            _ => Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_EA_ALLOW_NON_LOOPBACK",
                reason: "must be `true` or `false`",
            }),
        }
    }

    fn permits(self, address: IpAddr) -> bool {
        address.is_loopback() || self == Self::IsolatedNetwork
    }
}

impl BrokerSettings {
    /// Reads broker settings from the process environment.
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
    /// Returns [`ConfigError`] when the provider is unsupported, the token is
    /// missing or malformed, or a non-loopback bind lacks the explicit
    /// isolated-network opt-in.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Option<Self>, ConfigError> {
        let provider_raw = optional(&mut source, "VEYRA_BROKER_PROVIDER");
        let token_raw = optional(&mut source, "VEYRA_EA_TOKEN");
        let host_raw = optional(&mut source, "VEYRA_EA_BIND_HOST");
        let port_raw = optional(&mut source, "VEYRA_EA_BIND_PORT");
        let bind_policy_raw = optional(&mut source, "VEYRA_EA_ALLOW_NON_LOOPBACK");

        if provider_raw.is_empty() {
            if token_raw.is_empty()
                && host_raw.is_empty()
                && port_raw.is_empty()
                && bind_policy_raw.is_empty()
            {
                return Ok(None);
            }
            return Err(ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_BROKER_PROVIDER",
            });
        }

        let provider = BrokerProvider::parse(&provider_raw).ok_or(
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_BROKER_PROVIDER",
                reason: "unsupported provider; supported values: ea",
            },
        )?;

        match provider {
            BrokerProvider::Ea => {
                if token_raw.is_empty() {
                    return Err(ConfigError::MissingEnvironmentVariable {
                        name: "VEYRA_EA_TOKEN",
                    });
                }
                let token = EaToken::parse(&token_raw)?;

                let host = if host_raw.is_empty() {
                    "127.0.0.1".to_owned()
                } else {
                    host_raw
                };
                let bind_policy = EaBindPolicy::parse(&bind_policy_raw)?;
                let ip: IpAddr =
                    host.parse()
                        .map_err(|_| ConfigError::InvalidEnvironmentVariable {
                            name: "VEYRA_EA_BIND_HOST",
                            reason: "must be an IPv4 or IPv6 literal",
                        })?;
                if !bind_policy.permits(ip) {
                    return Err(ConfigError::InvalidEnvironmentVariable {
                        name: "VEYRA_EA_BIND_HOST",
                        reason: "must be loopback unless VEYRA_EA_ALLOW_NON_LOOPBACK=true is set for an isolated private network",
                    });
                }

                let port = if port_raw.is_empty() {
                    Port::parse("VEYRA_EA_BIND_PORT", "7801")?
                } else {
                    Port::parse("VEYRA_EA_BIND_PORT", &port_raw)?
                };

                Ok(Some(Self::Ea(EaSettings::new(
                    token,
                    SocketAddr::new(ip, port.value()),
                ))))
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "test-token-1234567890";

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
    fn absent_section_disables_broker_integration() {
        assert_eq!(BrokerSettings::from_source(source(&[])).expect("ok"), None);
    }

    #[test]
    fn partial_section_without_provider_is_rejected() {
        let error = BrokerSettings::from_source(source(&[("VEYRA_EA_TOKEN", TOKEN)]))
            .expect_err("partial settings must fail");
        assert!(matches!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_BROKER_PROVIDER"
            }
        ));
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let error = BrokerSettings::from_source(source(&[("VEYRA_BROKER_PROVIDER", "bridge")]))
            .expect_err("unknown provider must fail");
        assert!(matches!(
            error,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_BROKER_PROVIDER",
                ..
            }
        ));
    }

    #[test]
    fn ea_requires_a_token() {
        let error = BrokerSettings::from_source(source(&[("VEYRA_BROKER_PROVIDER", "ea")]))
            .expect_err("missing token must fail");
        assert!(matches!(
            error,
            ConfigError::MissingEnvironmentVariable {
                name: "VEYRA_EA_TOKEN"
            }
        ));
    }

    #[test]
    fn ea_defaults_to_loopback_port_7801() {
        let settings = BrokerSettings::from_source(source(&[
            ("VEYRA_BROKER_PROVIDER", "ea"),
            ("VEYRA_EA_TOKEN", TOKEN),
        ]))
        .expect("valid")
        .expect("configured");

        match settings {
            BrokerSettings::Ea(ea) => {
                assert_eq!(ea.bind(), "127.0.0.1:7801".parse().expect("addr"));
                assert_eq!(ea.stale_after(), Duration::from_secs(10));
                assert_eq!(ea.token().expose(), TOKEN);
            }
        }
    }

    #[test]
    fn ipv6_loopback_is_accepted() {
        let settings = BrokerSettings::from_source(source(&[
            ("VEYRA_BROKER_PROVIDER", "ea"),
            ("VEYRA_EA_TOKEN", TOKEN),
            ("VEYRA_EA_BIND_HOST", "::1"),
        ]))
        .expect("valid")
        .expect("configured");

        match settings {
            BrokerSettings::Ea(ea) => assert_eq!(ea.bind(), "[::1]:7801".parse().expect("addr")),
        }
    }

    #[test]
    fn non_loopback_and_malformed_binds_are_rejected() {
        let non_loopback = BrokerSettings::from_source(source(&[
            ("VEYRA_BROKER_PROVIDER", "ea"),
            ("VEYRA_EA_TOKEN", TOKEN),
            ("VEYRA_EA_BIND_HOST", "0.0.0.0"),
        ]))
        .expect_err("exposure must fail");
        assert!(matches!(
            non_loopback,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_EA_BIND_HOST",
                ..
            }
        ));

        let invalid_opt_in = BrokerSettings::from_source(source(&[
            ("VEYRA_BROKER_PROVIDER", "ea"),
            ("VEYRA_EA_TOKEN", TOKEN),
            ("VEYRA_EA_ALLOW_NON_LOOPBACK", "yes"),
        ]))
        .expect_err("malformed opt-in must fail");
        assert!(matches!(
            invalid_opt_in,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_EA_ALLOW_NON_LOOPBACK",
                ..
            }
        ));

        let malformed = BrokerSettings::from_source(source(&[
            ("VEYRA_BROKER_PROVIDER", "ea"),
            ("VEYRA_EA_TOKEN", TOKEN),
            ("VEYRA_EA_BIND_HOST", "localhost"),
        ]))
        .expect_err("dns names must fail");
        assert!(matches!(
            malformed,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_EA_BIND_HOST",
                ..
            }
        ));

        for port in ["80", "abc", "70000"] {
            let error = BrokerSettings::from_source(source(&[
                ("VEYRA_BROKER_PROVIDER", "ea"),
                ("VEYRA_EA_TOKEN", TOKEN),
                ("VEYRA_EA_BIND_PORT", port),
            ]))
            .expect_err("invalid port must fail");
            assert!(matches!(
                error,
                ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_EA_BIND_PORT",
                    ..
                }
            ));
        }
    }

    #[test]
    fn explicit_isolated_network_bind_is_accepted() {
        let settings = BrokerSettings::from_source(source(&[
            ("VEYRA_BROKER_PROVIDER", "ea"),
            ("VEYRA_EA_TOKEN", TOKEN),
            ("VEYRA_EA_BIND_HOST", "0.0.0.0"),
            ("VEYRA_EA_ALLOW_NON_LOOPBACK", "true"),
        ]))
        .expect("explicit isolated network bind should be valid")
        .expect("configured");

        match settings {
            BrokerSettings::Ea(ea) => {
                assert_eq!(ea.bind(), "0.0.0.0:7801".parse().expect("addr"));
            }
        }
    }

    #[test]
    fn short_tokens_are_rejected_and_debug_is_redacted() {
        assert!(EaToken::parse("short").is_err());
        let token = EaToken::parse(TOKEN).expect("valid");
        assert_eq!(format!("{token:?}"), "EaToken(redacted)");
        assert_eq!(
            format!(
                "{:?}",
                EaSettings::new(token, "127.0.0.1:7801".parse().expect("addr"))
            ),
            "EaSettings { token: EaToken(redacted), bind: 127.0.0.1:7801 }"
        );
    }
}
