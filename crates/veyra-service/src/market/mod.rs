//! Market-data integration boundary.
//!
//! Every market-data source implements [`MarketFeed`], so strategy and
//! reporting code depend on one narrow contract (validated candles) instead of
//! a vendor transport. The active implementation is selected by configuration
//! (`VEYRA_MARKET_PROVIDER`) and constructed once at startup by
//! [`MarketRuntime`]. The EA provider is the first implementation: it reads
//! closed candles straight from the terminal through the broker command
//! channel. Adding a REST or exchange provider means adding an implementation
//! plus a provider selector; callers do not change.

pub mod ea;
pub mod settings;

pub use settings::MarketSettings;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::broker::RatesRequest;
use crate::broker::{BrokerRuntime, Symbol, SymbolSpecPayload};

/// Supported market-data provider implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketProvider {
    /// MetaTrader 4 terminal, through the EA control channel.
    Ea,
}

impl MarketProvider {
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

impl fmt::Display for MarketProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Chart timeframe a candle series is aggregated at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeframe {
    /// One minute.
    M1,
    /// Five minutes.
    M5,
    /// Fifteen minutes.
    M15,
    /// Thirty minutes.
    M30,
    /// One hour.
    H1,
    /// Four hours.
    H4,
    /// One day.
    D1,
    /// One week.
    W1,
    /// One month.
    Mn1,
}

impl Timeframe {
    /// Every supported timeframe in ascending order.
    pub const ALL: [Timeframe; 9] = [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::M30,
        Timeframe::H1,
        Timeframe::H4,
        Timeframe::D1,
        Timeframe::W1,
        Timeframe::Mn1,
    ];

    /// Parses a timeframe name (`M1`…`MN1`, case-insensitive) or a standard
    /// MT4 period expressed in minutes (`240`).
    pub fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if let Ok(minutes) = trimmed.parse::<u32>() {
            return Self::from_minutes(minutes);
        }
        let name = trimmed.to_ascii_uppercase();
        Self::ALL
            .into_iter()
            .find(|timeframe| timeframe.as_str() == name)
    }

    /// Maps a standard MT4 period in minutes to its timeframe.
    pub fn from_minutes(minutes: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|timeframe| timeframe.minutes() == minutes)
    }

    /// Stable name used in configuration and output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::M1 => "M1",
            Self::M5 => "M5",
            Self::M15 => "M15",
            Self::M30 => "M30",
            Self::H1 => "H1",
            Self::H4 => "H4",
            Self::D1 => "D1",
            Self::W1 => "W1",
            Self::Mn1 => "MN1",
        }
    }

    /// Period length in minutes (the MT4 period constant).
    pub fn minutes(self) -> u32 {
        match self {
            Self::M1 => 1,
            Self::M5 => 5,
            Self::M15 => 15,
            Self::M30 => 30,
            Self::H1 => 60,
            Self::H4 => 240,
            Self::D1 => 1_440,
            Self::W1 => 10_080,
            Self::Mn1 => 43_200,
        }
    }
}

impl fmt::Display for Timeframe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One closed candle whose invariants were validated while parsing a provider
/// response: positive prices, `high` at or above the body prices and `low` at
/// or below them, and a non-negative tick volume.
#[derive(Debug, Clone, PartialEq)]
pub struct Candle {
    time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: i64,
}

impl Candle {
    /// Builds a candle from fields a provider already validated (see
    /// `broker::RatesPayload::validate`); never called on raw input.
    pub(crate) fn from_validated(
        time: i64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: i64,
    ) -> Self {
        Self {
            time,
            open,
            high,
            low,
            close,
            volume,
        }
    }

    /// Bar open time (Unix seconds, broker server time).
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Open price.
    pub fn open(&self) -> f64 {
        self.open
    }

    /// High price.
    pub fn high(&self) -> f64 {
        self.high
    }

    /// Low price.
    pub fn low(&self) -> f64 {
        self.low
    }

    /// Close price.
    pub fn close(&self) -> f64 {
        self.close
    }

    /// Tick volume reported by the provider.
    pub fn volume(&self) -> i64 {
        self.volume
    }
}

/// Validated market-data query.
#[derive(Debug, Clone, PartialEq)]
pub struct CandleRequest {
    symbol: Symbol,
    timeframe: Timeframe,
    bars: u16,
}

impl CandleRequest {
    /// Builds a query for `bars` closed candles.
    ///
    /// # Errors
    /// Returns [`MarketError::InvalidRequest`] when `bars` is zero or larger
    /// than the provider contract allows.
    pub fn new(symbol: Symbol, timeframe: Timeframe, bars: u16) -> Result<Self, MarketError> {
        if bars == 0 || bars > RatesRequest::MAX_BARS {
            return Err(MarketError::InvalidRequest {
                reason: format!("bars must be from 1 through {}", RatesRequest::MAX_BARS),
            });
        }
        Ok(Self {
            symbol,
            timeframe,
            bars,
        })
    }

    /// Instrument the candles are requested for.
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    /// Requested timeframe.
    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Requested number of closed candles.
    pub fn bars(&self) -> u16 {
        self.bars
    }
}

/// Validated window of closed candles, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct CandleSeries {
    symbol: Symbol,
    timeframe: Timeframe,
    candles: Vec<Candle>,
}

impl CandleSeries {
    /// Builds a series from candles a provider already validated; never called
    /// on raw input.
    pub(crate) fn from_validated(
        symbol: Symbol,
        timeframe: Timeframe,
        candles: Vec<Candle>,
    ) -> Self {
        Self {
            symbol,
            timeframe,
            candles,
        }
    }

    /// Instrument the candles belong to.
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    /// Timeframe the candles are aggregated at.
    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Closed candles, oldest first.
    pub fn candles(&self) -> &[Candle] {
        &self.candles
    }

    /// Most recent closed candle, when the series is non-empty.
    pub fn last(&self) -> Option<&Candle> {
        self.candles.last()
    }
}

/// Errors raised while constructing or using a market feed.
#[derive(Debug, thiserror::Error)]
pub enum MarketError {
    /// The provider implementation could not be constructed.
    #[error("market feed construction failed: {reason}")]
    Construction {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// The caller asked for an impossible window.
    #[error("invalid market request: {reason}")]
    InvalidRequest {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// The provider did not answer in time or reported a failure.
    #[error("market feed unavailable: {reason}")]
    Unavailable {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// A provider response violated the candle contract.
    #[error("market feed contract violation: {reason}")]
    Contract {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Narrow contract every market-data implementation implements.
#[async_trait]
pub trait MarketFeed: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output and logs.
    fn provider(&self) -> MarketProvider;

    /// Fetches the requested window of closed candles.
    ///
    /// # Errors
    /// Returns [`MarketError::Unavailable`] when the provider cannot answer
    /// and [`MarketError::Contract`] when its response is unusable.
    async fn candles(&self, request: CandleRequest) -> Result<CandleSeries, MarketError>;

    /// Fetches the venue's contract details for one instrument.
    ///
    /// # Errors
    /// Returns [`MarketError::Unavailable`] when the provider cannot answer
    /// and [`MarketError::Contract`] when its response is unusable.
    async fn symbol_spec(&self, symbol: &Symbol) -> Result<SymbolSpecPayload, MarketError>;
}

/// Active market-data integration selected by configuration.
#[derive(Debug, Clone)]
pub struct MarketRuntime {
    provider: MarketProvider,
    feed: Arc<dyn MarketFeed>,
}

impl MarketRuntime {
    /// Builds the implementation selected by settings.
    ///
    /// # Errors
    /// Returns [`MarketError::Construction`] when the selected implementation
    /// cannot be built from its settings or the active broker.
    pub fn from_settings(
        settings: MarketSettings,
        broker: Option<&BrokerRuntime>,
    ) -> Result<Self, MarketError> {
        match settings {
            MarketSettings::Ea(ea_settings) => {
                let link = broker.map(|runtime| runtime.link()).ok_or_else(|| {
                    MarketError::Construction {
                        reason: "the ea market provider requires an active broker command channel"
                            .to_owned(),
                    }
                })?;
                Ok(Self {
                    provider: MarketProvider::Ea,
                    feed: Arc::new(ea::EaMarketFeed::new(link, ea_settings.await_timeout())),
                })
            }
        }
    }

    /// Builds a runtime around an injected feed, deriving the provider from
    /// the feed itself. Used by tests and by embedders that construct their
    /// own implementation.
    pub fn from_feed(feed: Arc<dyn MarketFeed>) -> Self {
        let provider = feed.provider();
        Self { provider, feed }
    }

    /// Provider identifier of the active implementation.
    pub fn provider(&self) -> MarketProvider {
        self.provider
    }

    /// Domain-level feed contract used by strategy code.
    pub fn feed(&self) -> Arc<dyn MarketFeed> {
        self.feed.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::settings::EaSettings;
    use crate::broker::{BrokerSettings, EaToken};

    fn symbol() -> Symbol {
        Symbol::parse("EURUSD").expect("symbol")
    }

    #[test]
    fn provider_names_are_stable() {
        assert_eq!(MarketProvider::Ea.as_str(), "ea");
        assert_eq!(MarketProvider::parse(" EA "), Some(MarketProvider::Ea));
        assert_eq!(MarketProvider::parse("exchange"), None);
        assert_eq!(MarketProvider::Ea.to_string(), "ea");
    }

    #[test]
    fn timeframes_parse_by_name_and_minutes() {
        assert_eq!(Timeframe::parse("h4"), Some(Timeframe::H4));
        assert_eq!(Timeframe::parse(" 240 "), Some(Timeframe::H4));
        assert_eq!(Timeframe::parse("MN1"), Some(Timeframe::Mn1));
        assert_eq!(Timeframe::parse("90"), None);
        assert_eq!(Timeframe::parse("H6"), None);
        assert_eq!(Timeframe::H4.as_str(), "H4");
        assert_eq!(Timeframe::H4.minutes(), 240);
        assert_eq!(Timeframe::H4.to_string(), "H4");
        for timeframe in Timeframe::ALL {
            assert_eq!(
                Timeframe::from_minutes(timeframe.minutes()),
                Some(timeframe)
            );
        }
    }

    #[test]
    fn candle_requests_bound_the_window() {
        let request = CandleRequest::new(symbol(), Timeframe::H4, 48).expect("valid");
        assert_eq!(request.symbol().as_str(), "EURUSD");
        assert_eq!(request.timeframe(), Timeframe::H4);
        assert_eq!(request.bars(), 48);
        assert!(CandleRequest::new(symbol(), Timeframe::H4, 0).is_err());
        assert!(CandleRequest::new(symbol(), Timeframe::H4, 241).is_err());
    }

    #[test]
    fn series_accessors_expose_domain_values() {
        let candles = vec![
            Candle::from_validated(1_700_000_000, 1.1, 1.2, 1.0, 1.15, 42),
            Candle::from_validated(1_700_014_400, 1.15, 1.3, 1.1, 1.25, 77),
        ];
        let series = CandleSeries::from_validated(symbol(), Timeframe::H4, candles.clone());
        assert_eq!(series.symbol().as_str(), "EURUSD");
        assert_eq!(series.timeframe(), Timeframe::H4);
        assert_eq!(series.candles(), candles.as_slice());
        assert_eq!(series.last().expect("last").close(), 1.25);
        assert_eq!(series.last().expect("last").volume(), 77);
        assert_eq!(series.candles()[0].time(), 1_700_000_000);

        let empty = CandleSeries::from_validated(symbol(), Timeframe::H4, Vec::new());
        assert!(empty.last().is_none());
    }

    #[test]
    fn ea_market_settings_require_the_ea_broker() {
        let settings = MarketSettings::from_source(|name| match name {
            "VEYRA_MARKET_PROVIDER" => Ok("ea".to_owned()),
            _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        let error = MarketRuntime::from_settings(settings, None)
            .expect_err("ea feed without the ea broker is a construction error");
        assert!(
            error
                .to_string()
                .contains("requires an active broker command channel")
        );

        let broker = BrokerRuntime::from_settings(BrokerSettings::Ea(EaSettings::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            "127.0.0.1:0".parse().expect("address"),
        )))
        .expect("broker runtime");
        let settings = MarketSettings::from_source(|name| match name {
            "VEYRA_MARKET_PROVIDER" => Ok("ea".to_owned()),
            _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        let runtime =
            MarketRuntime::from_settings(settings, Some(&broker)).expect("runtime builds");
        assert_eq!(runtime.provider(), MarketProvider::Ea);
        assert_eq!(runtime.feed().provider(), MarketProvider::Ea);
    }

    #[test]
    fn injected_feeds_define_their_own_provider() {
        #[derive(Debug)]
        struct StubFeed;

        #[async_trait]
        impl MarketFeed for StubFeed {
            fn provider(&self) -> MarketProvider {
                MarketProvider::Ea
            }

            async fn candles(&self, _request: CandleRequest) -> Result<CandleSeries, MarketError> {
                Ok(CandleSeries::from_validated(
                    symbol(),
                    Timeframe::H4,
                    Vec::new(),
                ))
            }

            async fn symbol_spec(
                &self,
                _symbol: &Symbol,
            ) -> Result<SymbolSpecPayload, MarketError> {
                Err(MarketError::Unavailable {
                    reason: "stub has no contract data".to_owned(),
                })
            }
        }

        let runtime = MarketRuntime::from_feed(Arc::new(StubFeed));
        assert_eq!(runtime.provider(), MarketProvider::Ea);
    }
}
