//! Parses environment input once, before any listener is opened.
//! Bind addresses are IP literals, not DNS names; invalid input fails startup.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

/// Configuration failures name the setting without echoing its raw value.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Required input is absent, blank, or not valid Unicode.
    #[error("missing or unreadable environment variable: {name}")]
    MissingEnvironmentVariable {
        /// Setting that must be supplied.
        name: &'static str,
    },
    /// Input cannot be converted into a valid runtime setting.
    #[error("invalid environment variable `{name}`: {reason}")]
    InvalidEnvironmentVariable {
        /// Setting that must be corrected.
        name: &'static str,
        /// Non-sensitive acceptance rule.
        reason: &'static str,
    },
}

/// Default interval for periodic broker-state refresh.
const DEFAULT_RECONCILE_SECS: u64 = 30;
/// Largest refresh interval the parser accepts.
const MAX_RECONCILE_SECS: u64 = 3_600;

/// Deployment label; it does not grant execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    /// Production deployment.
    Production,
    /// Pre-production deployment.
    Staging,
    /// Local development.
    Development,
}

impl fmt::Display for Environment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Production => "production",
            Self::Staging => "staging",
            Self::Development => "development",
        })
    }
}

impl FromStr for Environment {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "production" => Ok(Self::Production),
            "staging" => Ok(Self::Staging),
            "development" => Ok(Self::Development),
            _ => Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_ENV",
                reason: "must be production, staging, or development",
            }),
        }
    }
}

/// Non-privileged TCP port. No deserializer can bypass the constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Port(u16);

impl Port {
    /// Parses an integer in 1024..=65535; rejects whitespace and port zero.
    /// `name` is the setting name used in error messages.
    pub fn parse(name: &'static str, value: &str) -> Result<Self, ConfigError> {
        let invalid = || ConfigError::InvalidEnvironmentVariable {
            name,
            reason: "must be an integer from 1024 through 65535",
        };
        let port = value.parse::<u16>().map_err(|_| invalid())?;
        if port < 1024 {
            return Err(invalid());
        }
        Ok(Self(port))
    }

    /// Returns the accepted TCP port.
    pub fn value(self) -> u16 {
        self.0
    }
}

/// Immutable startup settings with a parsed IPv4 or IPv6 socket address.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    address: SocketAddr,
    environment: Environment,
    trading_enabled: bool,
    reconcile_secs: u64,
    database_url: Option<String>,
}

impl ServiceConfig {
    /// Reads required process settings. A `.env` file is not loaded implicitly.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|name| {
            std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
        })
    }

    /// Parses an injected settings source; invalid or missing values fail closed.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let host = read("VEYRA_BIND_HOST", &mut source)?
            .parse::<IpAddr>()
            .map_err(|_| ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_BIND_HOST",
                reason: "must be an IPv4 or IPv6 literal",
            })?;
        let port = Port::parse("VEYRA_BIND_PORT", &read("VEYRA_BIND_PORT", &mut source)?)?;
        let environment = read("VEYRA_ENV", &mut source)?.parse()?;
        let trading_enabled = match source("VEYRA_TRADING_ENABLED") {
            Err(_) => false,
            Ok(value) => match value.trim() {
                "" | "false" => false,
                "true" => true,
                _ => {
                    return Err(ConfigError::InvalidEnvironmentVariable {
                        name: "VEYRA_TRADING_ENABLED",
                        reason: "must be `true` or `false`",
                    });
                }
            },
        };
        let reconcile_secs = match source("VEYRA_RECONCILE_SECS") {
            Err(_) => DEFAULT_RECONCILE_SECS,
            Ok(value) => match value.trim() {
                "" => DEFAULT_RECONCILE_SECS,
                other => {
                    let invalid = || ConfigError::InvalidEnvironmentVariable {
                        name: "VEYRA_RECONCILE_SECS",
                        reason: "must be an integer number of seconds from 0 through 3600",
                    };
                    let secs = other.parse::<u64>().map_err(|_| invalid())?;
                    if secs > MAX_RECONCILE_SECS {
                        return Err(invalid());
                    }
                    secs
                }
            },
        };
        let database_url = match source("VEYRA_DATABASE_URL") {
            Err(_) => None,
            Ok(value) => match value.trim() {
                "" => None,
                url => Some(url.to_owned()),
            },
        };
        Ok(Self {
            address: SocketAddr::new(host, port.value()),
            environment,
            trading_enabled,
            reconcile_secs,
            database_url,
        })
    }

    /// Returns a validated bind address; IPv6 requires no string concatenation.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Returns the deployment label, not a trading permission.
    pub fn environment(&self) -> Environment {
        self.environment
    }

    /// PostgreSQL connection string for the audit trail, when configured.
    pub fn database_url(&self) -> Option<&str> {
        self.database_url.as_deref()
    }

    /// Interval between periodic broker-state refreshes; zero disables them.
    pub fn reconcile_secs(&self) -> u64 {
        self.reconcile_secs
    }

    /// Whether the operator has explicitly enabled execution request paths.
    ///
    /// This is the first of two independent controls in front of real money:
    /// the terminal additionally refuses to trade until its own live-orders
    /// input is enabled.
    pub fn trading_enabled(&self) -> bool {
        self.trading_enabled
    }
}

fn read(
    name: &'static str,
    source: &mut impl FnMut(&'static str) -> Result<String, ConfigError>,
) -> Result<String, ConfigError> {
    let value = source(name)?;
    if value.trim().is_empty() {
        return Err(ConfigError::MissingEnvironmentVariable { name });
    }
    Ok(value)
}
