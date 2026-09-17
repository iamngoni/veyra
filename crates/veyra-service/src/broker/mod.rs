//! Broker integration boundary.
//!
//! Every venue integration implements [`BrokerLink`], so the decision, risk,
//! and reporting layers depend on one narrow contract instead of a transport.
//! The active implementation is selected by configuration
//! (`VEYRA_BROKER_PROVIDER`) and constructed once at startup by
//! [`BrokerRuntime`]. Adding a venue means adding an implementation plus a
//! provider selector; no caller changes.

pub mod command;
pub mod ea;
pub mod settings;

/// Provider-neutral command and report types every venue integration speaks.
pub use command::{
    AccountSnapshotPayload, CandlePayload, CloseOrderRequest, CommandId, CommandKind,
    CommandPayload, CommandRecord, CommandState, ListedCommand, ModifyOrderRequest, ORDER_MAGIC,
    OrderCheckPayload, OrderExecutionPayload, OrderRequest, PositionKind, PositionPayload,
    RatesPayload, RatesRequest, SUPPORTED_TIMEFRAME_MINUTES,
};
/// EA-specific transport surface, used by the EA server and its contract tests.
pub use ea::{EaErrorBody, EaLink, EaPoll, EaReply, build_server, create_ea_app};
pub use settings::{BrokerSettings, EaToken};

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::audit::AuditRuntime;

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
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSnapshot {
    login: AccountLogin,
    server: ServerName,
    symbol: Symbol,
    connected: bool,
    trade_allowed: bool,
    live_orders: bool,
    open_orders: u32,
    open_lots: f64,
}

impl AccountSnapshot {
    /// Builds a snapshot from already validated components; the terminal
    /// starts assumed disarmed until [`AccountSnapshot::with_live_orders`]
    /// reports otherwise.
    pub fn new(
        login: AccountLogin,
        server: ServerName,
        symbol: Symbol,
        connected: bool,
        trade_allowed: bool,
        open_orders: u32,
        open_lots: f64,
    ) -> Self {
        Self {
            login,
            server,
            symbol,
            connected,
            trade_allowed,
            live_orders: false,
            open_orders,
            open_lots,
        }
    }

    /// Records whether the terminal's EA is armed for live orders.
    pub fn with_live_orders(mut self, live_orders: bool) -> Self {
        self.live_orders = live_orders;
        self
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

    /// Whether the terminal's EA is compiled and attached armed for live
    /// orders (`InAllowLiveOrders`). The second of the two execution controls.
    pub fn live_orders(&self) -> bool {
        self.live_orders
    }

    /// Number of open venue orders (MT4 counts positions and pending orders).
    pub fn open_orders(&self) -> u32 {
        self.open_orders
    }

    /// Total open volume in lots across every open order.
    pub fn open_lots(&self) -> f64 {
        self.open_lots
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

/// Contract every venue integration implements.
///
/// Two halves: reporting (`report`, retained `account_snapshot` state) and the
/// asynchronous command channel (`enqueue_*`, `command`, `await_command`).
/// Implementations must not block on network IO inside these methods — the
/// venue is pumped by its own transport (the EA polls; a REST venue would run
/// its own task) and every method only touches locally held state.
///
/// The trading, risk, control, and console layers depend on this trait alone,
/// so adding a venue means adding one implementation plus a provider selector;
/// no caller changes.
#[async_trait]
pub trait BrokerLink: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output and logs.
    fn provider(&self) -> BrokerProvider;

    /// Latest link report.
    async fn report(&self) -> LinkReport;

    /// Queues a read-only account snapshot.
    fn enqueue_account_snapshot(&self) -> CommandId;

    /// Queues a broker-side order validation (never places an order).
    fn enqueue_order_check(&self, request: OrderRequest) -> CommandId;

    /// Queues a live order for a gate-approved intent.
    fn enqueue_open_order(&self, request: OrderRequest) -> CommandId;

    /// Queues a close for one validated Veyra-owned ticket.
    fn enqueue_close_order(&self, request: CloseOrderRequest) -> CommandId;

    /// Queues a stop change for one validated Veyra-owned ticket.
    fn enqueue_modify_order(&self, request: ModifyOrderRequest) -> CommandId;

    /// Queues a read-only market-rates request.
    fn enqueue_rates(&self, request: RatesRequest) -> CommandId;

    /// Whether a command of `kind` is still awaiting acknowledgement.
    fn has_pending(&self, kind: CommandKind) -> bool;

    /// Newest-first commands for the control surface, capped at `limit`.
    fn recent_commands(&self, limit: usize) -> Vec<ListedCommand>;

    /// Current record for a command inside the bounded history.
    fn command(&self, id: CommandId) -> Option<CommandRecord>;

    /// Waits until a command reaches a terminal state; the transport owns
    /// delivery and timeout classification.
    async fn await_command(&self, id: CommandId, timeout: Duration) -> CommandState;

    /// Latest validated account snapshot, if the venue ever reported one.
    fn last_account(&self) -> Option<AccountSnapshotPayload>;

    /// Age of the latest validated snapshot, if there is one.
    fn last_account_age(&self, now: SystemTime) -> Option<Duration>;

    /// Attaches the audit trail so command lifecycle events are recorded.
    /// Providers that already emit equivalent events may ignore this.
    fn attach_audit(&self, _audit: Arc<AuditRuntime>) {}
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
                    ea_settings.command_timeout(),
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

    /// A second implementation with no EA transport at all. Its existence is
    /// the seam test: the generic layers compile against `dyn BrokerLink`, so a
    /// new venue only has to answer this contract.
    #[derive(Debug)]
    struct StubLink;

    #[async_trait]
    impl BrokerLink for StubLink {
        fn provider(&self) -> BrokerProvider {
            BrokerProvider::Ea
        }

        async fn report(&self) -> LinkReport {
            LinkReport {
                snapshot: None,
                fresh: false,
            }
        }

        fn enqueue_account_snapshot(&self) -> CommandId {
            CommandId::new()
        }

        fn enqueue_order_check(&self, _request: OrderRequest) -> CommandId {
            CommandId::new()
        }

        fn enqueue_open_order(&self, _request: OrderRequest) -> CommandId {
            CommandId::new()
        }

        fn enqueue_close_order(&self, _request: CloseOrderRequest) -> CommandId {
            CommandId::new()
        }

        fn enqueue_modify_order(&self, _request: ModifyOrderRequest) -> CommandId {
            CommandId::new()
        }

        fn enqueue_rates(&self, _request: RatesRequest) -> CommandId {
            CommandId::new()
        }

        fn has_pending(&self, _kind: CommandKind) -> bool {
            false
        }

        fn recent_commands(&self, _limit: usize) -> Vec<ListedCommand> {
            Vec::new()
        }

        fn command(&self, _id: CommandId) -> Option<CommandRecord> {
            None
        }

        async fn await_command(&self, _id: CommandId, _timeout: Duration) -> CommandState {
            CommandState::Failed {
                reason: "stub".to_owned(),
            }
        }

        fn last_account(&self) -> Option<AccountSnapshotPayload> {
            None
        }

        fn last_account_age(&self, _now: SystemTime) -> Option<Duration> {
            None
        }
    }

    #[actix_web::test]
    async fn a_non_ea_link_satisfies_the_generic_contract() {
        use crate::audit::{AuditRuntime, MemoryTrail};

        let link: Arc<dyn BrokerLink> = Arc::new(StubLink);
        assert!(!link.report().await.fresh);
        assert!(link.last_account().is_none());
        assert!(link.command(CommandId::new()).is_none());
        assert!(!link.has_pending(CommandKind::OpenOrder));
        link.attach_audit(Arc::new(AuditRuntime::new(
            Arc::new(MemoryTrail::default()),
        )));
        assert!(matches!(
            link.await_command(CommandId::new(), Duration::from_millis(1))
                .await,
            CommandState::Failed { .. }
        ));
    }

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
