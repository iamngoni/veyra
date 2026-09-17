//! Broker integration boundary.
//!
//! Every venue integration implements [`BrokerLink`], so the decision, risk,
//! and reporting layers depend on one narrow contract instead of a transport.
//! The active implementation is selected by configuration
//! (`VEYRA_BROKER_PROVIDER`) and constructed once at startup by
//! [`BrokerRuntime`]. Adding a venue means adding an implementation plus a
//! provider selector; no caller changes.

pub mod ea;
pub mod settings;

pub use ea::{EaErrorBody, EaLink, EaPoll, EaReply, build_server, create_ea_app};
pub use settings::{BrokerSettings, EaToken};

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

/// Errors raised while validating venue data or constructing a link.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BrokerError {
    /// A venue payload failed validation.
    #[error("invalid broker payload field `{field}`: {reason}")]
    InvalidPayload {
        /// Field that failed validation.
        field: &'static str,
        /// Why the value was rejected.
        reason: &'static str,
    },
    /// A link implementation could not be constructed.
    #[error("broker link construction failed: {reason}")]
    Construction {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Supported broker integration implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerProvider {
    /// MetaTrader 4 terminal reached through the in-terminal EA control channel.
    Ea,
}

impl BrokerProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ea => "ea",
        }
    }

    /// Parses a configuration value; unknown providers are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ea" => Some(Self::Ea),
            _ => None,
        }
    }
}

impl fmt::Display for BrokerProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Validated venue server name (for example `IFCMarkets-Real`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerName(String);

impl ServerName {
    /// Parses a server name: 1-64 characters of `[A-Za-z0-9._-]`.
    pub fn parse(value: &str) -> Result<Self, BrokerError> {
        let trimmed = value.trim();
        let valid = !trimmed.is_empty()
            && trimmed.len() <= 64
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if valid {
            Ok(Self(trimmed.to_owned()))
        } else {
            Err(BrokerError::InvalidPayload {
                field: "server",
                reason: "must be 1-64 characters of letters, digits, '.', '_' or '-'",
            })
        }
    }

    /// Returns the validated name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validated trading account number; account numbers are positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountLogin(u64);

impl AccountLogin {
    /// Parses a strictly positive account number.
    pub fn parse(value: i64) -> Result<Self, BrokerError> {
        let invalid = || BrokerError::InvalidPayload {
            field: "acct",
            reason: "must be a positive account number",
        };
        let value = u64::try_from(value).map_err(|_| invalid())?;
        if value == 0 {
            return Err(invalid());
        }
        Ok(Self(value))
    }

    /// Returns the account number.
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Validated instrument symbol (for example `EURUSD`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol(String);

impl Symbol {
    /// Parses a symbol: 1-24 characters of `[A-Za-z0-9._#+-]`.
    pub fn parse(value: &str) -> Result<Self, BrokerError> {
        let trimmed = value.trim();
        let valid = !trimmed.is_empty()
            && trimmed.len() <= 24
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '#' | '+' | '-'));
        if valid {
            Ok(Self(trimmed.to_owned()))
        } else {
            Err(BrokerError::InvalidPayload {
                field: "symbol",
                reason: "must be 1-24 characters of letters, digits, '.', '_', '#', '+' or '-'",
            })
        }
    }

    /// Returns the validated symbol.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Account and terminal state observed from the venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    login: AccountLogin,
    server: ServerName,
    symbol: Symbol,
    connected: bool,
    trade_allowed: bool,
}

impl AccountSnapshot {
    /// Builds a snapshot from already validated components.
    pub fn new(
        login: AccountLogin,
        server: ServerName,
        symbol: Symbol,
        connected: bool,
        trade_allowed: bool,
    ) -> Self {
        Self {
            login,
            server,
            symbol,
            connected,
            trade_allowed,
        }
    }

    /// Account number.
    pub fn login(&self) -> AccountLogin {
        self.login
    }

    /// Broker server name.
    pub fn server(&self) -> &ServerName {
        &self.server
    }

    /// Symbol of the chart hosting the terminal side of the link.
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    /// Whether the terminal currently reports a broker connection.
    pub fn connected(&self) -> bool {
        self.connected
    }

    /// Whether the terminal currently allows trading operations.
    pub fn trade_allowed(&self) -> bool {
        self.trade_allowed
    }
}

/// Current link state: the latest snapshot, if any, and whether it is fresh.
#[derive(Debug, Clone)]
pub struct LinkReport {
    /// Latest observed account state, if the venue has ever reported one.
    pub snapshot: Option<AccountSnapshot>,
    /// Whether the state was observed within the freshness window.
    pub fresh: bool,
}

/// Narrow contract every venue integration implements.
///
/// Implementations must not block on network IO inside [`BrokerLink::report`];
/// they report the latest locally held state. Outbound calls belong to the
/// execution methods that will be added with the command layer.
#[async_trait]
pub trait BrokerLink: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output and logs.
    fn provider(&self) -> BrokerProvider;

    /// Latest link report.
    async fn report(&self) -> LinkReport;
}

/// Active broker integration plus the concrete implementation's extras.
#[derive(Debug, Clone)]
pub struct BrokerRuntime {
    provider: BrokerProvider,
    link: Arc<dyn BrokerLink>,
    ea: Option<(Arc<EaLink>, std::net::SocketAddr)>,
}

impl BrokerRuntime {
    /// Builds the implementation selected by configuration.
    ///
    /// # Errors
    /// Returns [`BrokerError`] when the selected implementation cannot be
    /// constructed from its settings.
    pub fn from_settings(settings: BrokerSettings) -> Result<Self, BrokerError> {
        match settings {
            BrokerSettings::Ea(ea_settings) => {
                let link = Arc::new(EaLink::new(
                    ea_settings.token().clone(),
                    ea_settings.stale_after(),
                ));
                Ok(Self {
                    provider: BrokerProvider::Ea,
                    link: link.clone(),
                    ea: Some((link, ea_settings.bind())),
                })
            }
        }
    }

    /// Provider identifier of the active implementation.
    pub fn provider(&self) -> BrokerProvider {
        self.provider
    }

    /// Domain-level link contract used by decision and risk layers.
    pub fn link(&self) -> Arc<dyn BrokerLink> {
        self.link.clone()
    }

    /// EA link handle, present only when the EA provider is active.
    pub fn ea_link(&self) -> Option<Arc<EaLink>> {
        self.ea.as_ref().map(|(link, _)| link.clone())
    }

    /// Loopback listener this provider requires, if any.
    ///
    /// # Errors
    /// Returns IO errors from binding the provider's listener.
    pub fn listener(&self) -> Result<Option<actix_web::dev::Server>, std::io::Error> {
        match &self.ea {
            Some((link, address)) => Ok(Some(ea::build_server(link.clone(), *address)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_accepts_and_rejects() {
        assert_eq!(
            ServerName::parse(" IFCMarkets-Real ")
                .expect("valid")
                .as_str(),
            "IFCMarkets-Real"
        );
        for bad in ["", "  ", "has space", "sneaky/../path", &"A".repeat(65)] {
            assert!(ServerName::parse(bad).is_err(), "must reject: {bad:?}");
        }
    }

    #[test]
    fn account_login_requires_positive() {
        assert_eq!(AccountLogin::parse(94168).expect("valid").value(), 94168);
        assert!(AccountLogin::parse(0).is_err());
        assert!(AccountLogin::parse(-1).is_err());
    }

    #[test]
    fn symbol_accepts_and_rejects() {
        assert_eq!(Symbol::parse(" EURUSD ").expect("valid").as_str(), "EURUSD");
        assert_eq!(
            Symbol::parse("US100.cash").expect("valid").as_str(),
            "US100.cash"
        );
        for bad in ["", "  ", "has space", "bad$char", &"A".repeat(25)] {
            assert!(Symbol::parse(bad).is_err(), "must reject: {bad:?}");
        }
    }

    #[test]
    fn provider_parses_known_values_only() {
        assert_eq!(BrokerProvider::parse(" EA "), Some(BrokerProvider::Ea));
        assert_eq!(BrokerProvider::parse("bridge"), None);
        assert_eq!(BrokerProvider::Ea.as_str(), "ea");
        assert_eq!(BrokerProvider::Ea.to_string(), "ea");
    }
}
