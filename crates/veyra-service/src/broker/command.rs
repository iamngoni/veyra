//! Provider-neutral broker command and report contract.
//!
//! These types are what every venue integration must speak: command ids,
//! lifecycle states, requests derived from an approved intent, and validated
//! reports from the venue. The MetaTrader EA implementation happens to encode
//! them as its wire format; another provider maps them to its own protocol.
//! Nothing here references a transport, so the trading, risk, control, and
//! console layers stay independent of the venue behind [`crate::broker::BrokerLink`].

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::broker::{BrokerError, Symbol};
use crate::trading::intent::TradeIntent;

/// Stable identifier for one command; identical across delivery and ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(Uuid);

impl CommandId {
    /// Generates a fresh random identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parses an externally supplied identifier (for example a URL path).
    pub fn parse(value: &str) -> Option<Self> {
        Uuid::parse_str(value).ok().map(Self)
    }
}

impl Default for CommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Commands the EA can execute. None of them places, modifies, or cancels an
/// order; `order_check` only asks the terminal to validate a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    /// Return-path check with no payload.
    Ping,
    /// Report account state (balance, equity, free margin, order count).
    AccountSnapshot,
    /// Ask the terminal to validate an order request without sending it.
    OrderCheck,
    /// Ask the terminal to execute an order (subject to the terminal's own
    /// live-orders control).
    OpenOrder,
    /// Ask the terminal to close one Veyra-owned market position.
    CloseOrder,
    /// Ask the terminal to change the stops on a Veyra-owned position.
    ModifyOrder,
    /// Report recent closed candles for one symbol and timeframe.
    Rates,
    /// Report the venue's contract details for one instrument (spread, stop
    /// level, lot band, margin requirement, swap rates).
    SymbolSpec,
    /// Report closed orders from the terminal's account history.
    OrderHistory,
}

impl CommandKind {
    /// Returns the stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::AccountSnapshot => "account_snapshot",
            Self::OrderCheck => "order_check",
            Self::OpenOrder => "open_order",
            Self::CloseOrder => "close_order",
            Self::ModifyOrder => "modify_order",
            Self::Rates => "rates",
            Self::SymbolSpec => "symbol_spec",
            Self::OrderHistory => "order_history",
        }
    }
}

/// Largest position list the payload accepts; the terminal caps earlier and
/// flags truncation.
const MAX_POSITIONS: usize = 64;

/// Magic number stamped on Veyra orders so the terminal and the reconciler can
/// recognise them.
pub const ORDER_MAGIC: u32 = 77_041;

/// Standard MT4 periods in minutes; the `rates` contract accepts only these.
pub const SUPPORTED_TIMEFRAME_MINUTES: [u32; 9] = [1, 5, 15, 30, 60, 240, 1_440, 10_080, 43_200];

/// One open or pending order as the terminal reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionPayload {
    /// Venue ticket.
    pub ticket: i64,
    /// Instrument.
    pub symbol: String,
    /// Magic number stamped on the order; [`ORDER_MAGIC`] marks Veyra orders.
    pub magic: u32,
    /// Order kind.
    pub kind: PositionKind,
    /// Volume in lots.
    pub lots: f64,
    /// Entry or trigger price.
    pub price: f64,
    /// Floating profit in account currency.
    pub profit: f64,
    /// Stop loss as an absolute price, or zero when the position carries
    /// none. Absent on older terminals that do not report it.
    #[serde(rename = "sl", default)]
    pub stop_loss: f64,
    /// Take profit as an absolute price, or zero when the position carries
    /// none. Absent on older terminals that do not report it.
    #[serde(rename = "tp", default)]
    pub take_profit: f64,
    /// Position open time (broker server seconds), or zero when the terminal
    /// does not report it. The autopilot refuses to close positions whose age
    /// it cannot verify.
    #[serde(rename = "openedAt", default)]
    pub opened_at: i64,
    /// Current close price for the position, or zero when the terminal does
    /// not report it. The break-even policy is skipped without it.
    #[serde(default)]
    pub current: f64,
    /// Swap charged or credited on the position so far, in account currency.
    /// Absent on older terminals that do not report it.
    #[serde(default)]
    pub swap: f64,
}

/// Order kinds the terminal can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionKind {
    /// Market buy position.
    Buy,
    /// Market sell position.
    Sell,
    /// Buy limit order.
    BuyLimit,
    /// Sell limit order.
    SellLimit,
    /// Buy stop order.
    BuyStop,
    /// Sell stop order.
    SellStop,
    /// Buy stop-limit order.
    BuyStopLimit,
    /// Sell stop-limit order.
    SellStopLimit,
}

/// Account state reported by an `account_snapshot` command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountSnapshotPayload {
    /// Account balance.
    pub balance: f64,
    /// Account equity.
    pub equity: f64,
    /// Free margin.
    #[serde(rename = "freeMargin")]
    pub free_margin: f64,
    /// Open orders; MT4 counts positions and pending orders here.
    pub orders: u32,
    /// Total open volume across every open order, in lots.
    pub lots: f64,
    /// Bounded snapshot of the open orders.
    pub positions: Vec<PositionPayload>,
    /// Whether the terminal omitted orders beyond its own cap.
    #[serde(rename = "positionsTruncated")]
    pub positions_truncated: bool,
    /// Terminal server time.
    #[serde(rename = "serverTime")]
    pub server_time: i64,
    /// Account leverage (for example 100 for 1:100), or zero when the
    /// terminal does not report it.
    #[serde(default)]
    pub leverage: u32,
    /// Margin level percentage (equity / used margin x 100), or zero when no
    /// margin is used or the terminal does not report it.
    #[serde(rename = "marginLevel", default)]
    pub margin_level: f64,
}

impl AccountSnapshotPayload {
    /// Rejects non-finite money values and unusable exposure data before they
    /// reach callers.
    pub(crate) fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("balance", self.balance),
            ("equity", self.equity),
            ("freeMargin", self.free_margin),
            ("marginLevel", self.margin_level),
        ] {
            if !value.is_finite() {
                return Err(format!("{name} must be a finite number"));
            }
        }
        if !self.lots.is_finite() || self.lots < 0.0 {
            return Err("lots must be a finite, non-negative number".to_owned());
        }
        if self.margin_level < 0.0 {
            return Err("marginLevel must be non-negative".to_owned());
        }
        if self.positions.len() > MAX_POSITIONS {
            return Err(format!(
                "positions must contain at most {MAX_POSITIONS} entries"
            ));
        }
        for position in &self.positions {
            if position.ticket <= 0 {
                return Err("position ticket must be positive".to_owned());
            }
            Symbol::parse(&position.symbol)
                .map_err(|error| format!("position symbol is invalid: {error}"))?;
            if !position.lots.is_finite() || position.lots <= 0.0 {
                return Err("position lots must be a finite, positive number".to_owned());
            }
            if !position.price.is_finite() || position.price <= 0.0 {
                return Err("position price must be a finite, positive number".to_owned());
            }
            if !position.profit.is_finite() {
                return Err("position profit must be a finite number".to_owned());
            }
            for (name, value) in [("sl", position.stop_loss), ("tp", position.take_profit)] {
                if !value.is_finite() || value < 0.0 {
                    return Err(format!(
                        "position {name} must be a finite, non-negative price (zero means none)"
                    ));
                }
            }
            if position.opened_at < 0 {
                return Err("position openedAt must be non-negative".to_owned());
            }
            if !position.current.is_finite() || position.current < 0.0 {
                return Err("position current must be a finite, non-negative price".to_owned());
            }
            if !position.swap.is_finite() {
                return Err("position swap must be a finite number".to_owned());
            }
        }
        Ok(())
    }
}

/// Terminal verdict for an `order_check` command: the request passed, or the
/// classic MT4 trade code (131 volume, 134 money, 130 stops, 133 disabled)
/// that would reject it. No order exists in the venue; the terminal applies
/// its own market rules and margin engine.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OrderCheckPayload {
    /// Whether the terminal accepted the request in principle.
    pub passed: bool,
    /// Classic MT4 trade code; zero when the check passed.
    pub retcode: i64,
    /// Broker explanation, echoed for operators.
    pub comment: String,
    /// Margin the venue would require for the order, in account currency.
    pub margin: f64,
}

impl OrderCheckPayload {
    /// Rejects values an operator must never act on.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.margin.is_finite() || self.margin < 0.0 {
            return Err("margin must be a finite, non-negative number".to_owned());
        }
        if self.comment.len() > 256 || self.comment.chars().any(char::is_control) {
            return Err(
                "comment must be at most 256 characters without control characters".to_owned(),
            );
        }
        Ok(())
    }
}

/// Terminal verdict for an `open_order` command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OrderExecutionPayload {
    /// Whether an order was actually sent to the broker.
    pub executed: bool,
    /// Validation or broker return code; zero means the request was acceptable.
    pub retcode: i64,
    /// Terminal commentary, echoed for operators.
    pub comment: String,
    /// Ticket of the placed order; zero when nothing was sent.
    pub ticket: i64,
    /// Fill or trigger price; zero when nothing was sent.
    pub price: f64,
}

impl OrderExecutionPayload {
    /// Rejects values an operator must never act on.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.comment.len() > 256 || self.comment.chars().any(char::is_control) {
            return Err(
                "comment must be at most 256 characters without control characters".to_owned(),
            );
        }
        if self.ticket < 0 {
            return Err("ticket must not be negative".to_owned());
        }
        if !self.price.is_finite() || self.price < 0.0 {
            return Err("price must be a finite, non-negative number".to_owned());
        }
        if self.executed && (self.ticket <= 0 || self.price <= 0.0) {
            return Err("an executed order must report a ticket and price".to_owned());
        }
        Ok(())
    }
}

/// Close request sent to the EA: one Veyra-owned ticket plus the magic number
/// the terminal must confirm before touching it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloseOrderRequest {
    ticket: i64,
    magic: u32,
}

impl CloseOrderRequest {
    /// Builds a close request for a validated, Veyra-owned ticket.
    pub fn new(ticket: i64, magic: u32) -> Self {
        Self { ticket, magic }
    }

    /// Ticket to close.
    pub fn ticket(&self) -> i64 {
        self.ticket
    }

    /// Magic number the terminal must find on the selected order.
    pub fn magic(&self) -> u32 {
        self.magic
    }
}

/// Stop-change request sent to the EA for one Veyra-owned ticket. At least one
/// of the two stops must be present; the terminal re-validates distances.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModifyOrderRequest {
    ticket: i64,
    magic: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_loss: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    take_profit: Option<f64>,
}

impl ModifyOrderRequest {
    /// Builds a stop change for a validated, Veyra-owned ticket.
    pub fn new(ticket: i64, magic: u32, stop_loss: Option<f64>, take_profit: Option<f64>) -> Self {
        Self {
            ticket,
            magic,
            stop_loss,
            take_profit,
        }
    }

    /// Ticket whose stops change.
    pub fn ticket(&self) -> i64 {
        self.ticket
    }

    /// Magic number the terminal must find on the selected order.
    pub fn magic(&self) -> u32 {
        self.magic
    }

    /// New stop loss, when provided.
    pub fn stop_loss(&self) -> Option<f64> {
        self.stop_loss
    }

    /// New take profit, when provided.
    pub fn take_profit(&self) -> Option<f64> {
        self.take_profit
    }
}

/// Market-rates request sent to the EA: `bars` closed candles for a symbol
/// and timeframe, oldest first. The symbol is an already validated [`Symbol`]
/// and the timeframe must be one of [`SUPPORTED_TIMEFRAME_MINUTES`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RatesRequest {
    symbol: String,
    #[serde(rename = "timeframeMinutes")]
    timeframe_minutes: u32,
    bars: u16,
}

impl RatesRequest {
    /// Largest candle count one request may ask for.
    pub const MAX_BARS: u16 = 240;

    /// Builds a validated request.
    ///
    /// # Errors
    /// Returns [`BrokerError::InvalidPayload`] when the timeframe is not a
    /// standard MT4 period or the bar count is outside 1-240.
    pub fn new(symbol: &Symbol, timeframe_minutes: u32, bars: u16) -> Result<Self, BrokerError> {
        if !SUPPORTED_TIMEFRAME_MINUTES.contains(&timeframe_minutes) {
            return Err(BrokerError::InvalidPayload {
                field: "timeframeMinutes",
                reason: "must be a standard MT4 period in minutes (1, 5, 15, 30, 60, 240, 1440, 10080, 43200)",
            });
        }
        if bars == 0 || bars > Self::MAX_BARS {
            return Err(BrokerError::InvalidPayload {
                field: "bars",
                reason: "must be from 1 through 240",
            });
        }
        Ok(Self {
            symbol: symbol.as_str().to_owned(),
            timeframe_minutes,
            bars,
        })
    }

    /// Instrument the candles are requested for.
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// Requested timeframe in minutes.
    pub fn timeframe_minutes(&self) -> u32 {
        self.timeframe_minutes
    }

    /// Requested number of closed candles.
    pub fn bars(&self) -> u16 {
        self.bars
    }
}

/// One closed OHLC candle as the terminal reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandlePayload {
    /// Bar open time (Unix seconds, broker server time).
    pub time: i64,
    /// Open price.
    pub open: f64,
    /// High price.
    pub high: f64,
    /// Low price.
    pub low: f64,
    /// Close price.
    pub close: f64,
    /// Tick volume reported by MT4.
    pub volume: i64,
}

impl CandlePayload {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.time <= 0 {
            return Err("candle time must be positive".to_owned());
        }
        for (name, value) in [
            ("open", self.open),
            ("high", self.high),
            ("low", self.low),
            ("close", self.close),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("candle {name} must be a finite, positive price"));
            }
        }
        if self.high < self.low {
            return Err("candle high must not be below its low".to_owned());
        }
        if self.high < self.open.max(self.close) {
            return Err("candle high must not be below its body prices".to_owned());
        }
        if self.low > self.open.min(self.close) {
            return Err("candle low must not be above its body prices".to_owned());
        }
        if self.volume < 0 {
            return Err("candle volume must be non-negative".to_owned());
        }
        Ok(())
    }
}

/// Result of a `rates` command: the requested window of closed candles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatesPayload {
    /// Instrument the candles belong to.
    pub symbol: String,
    /// Timeframe in minutes.
    #[serde(rename = "timeframeMinutes")]
    pub timeframe_minutes: u32,
    /// Closed candles, oldest first.
    pub candles: Vec<CandlePayload>,
}

impl RatesPayload {
    /// Rejects unusable series before they reach callers: unknown
    /// symbol/timeframe, an empty or oversized series, non-monotonic times,
    /// or any candle that fails OHLC sanity. Market-feed implementations call
    /// this again when converting to domain types, so a hand-built payload
    /// cannot bypass the checks.
    pub(crate) fn validate(&self) -> Result<(), String> {
        Symbol::parse(&self.symbol).map_err(|error| format!("rates symbol is invalid: {error}"))?;
        if !SUPPORTED_TIMEFRAME_MINUTES.contains(&self.timeframe_minutes) {
            return Err("rates timeframeMinutes is not a standard MT4 period".to_owned());
        }
        let max = usize::from(RatesRequest::MAX_BARS);
        if self.candles.is_empty() || self.candles.len() > max {
            return Err(format!("rates candles must be 1-{max} entries"));
        }
        let mut previous = None;
        for candle in &self.candles {
            candle.validate()?;
            if previous.is_some_and(|previous| candle.time <= previous) {
                return Err("candle times must be strictly increasing".to_owned());
            }
            previous = Some(candle.time);
        }
        Ok(())
    }
}

/// Symbol-spec request sent to the EA: the venue contract for one instrument.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolSpecRequest {
    symbol: String,
}

impl SymbolSpecRequest {
    /// Builds a request for an already validated instrument.
    pub fn new(symbol: &Symbol) -> Self {
        Self {
            symbol: symbol.as_str().to_owned(),
        }
    }

    /// Instrument the contract is requested for.
    pub fn symbol(&self) -> &str {
        &self.symbol
    }
}

/// Venue contract details for one instrument as the terminal reports them.
///
/// These values price risk locally: the spread and minimum stop distance
/// decide whether a stop can survive execution, the lot band gates volume, and
/// the margin requirement pre-checks affordability before an order is queued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolSpecPayload {
    /// Instrument the contract belongs to.
    pub symbol: String,
    /// Price digits.
    pub digits: u32,
    /// Smallest price increment.
    pub point: f64,
    /// Current spread in points.
    #[serde(rename = "spreadPoints")]
    pub spread_points: u32,
    /// Broker minimum distance between the market price and a stop, in points.
    #[serde(rename = "stopLevelPoints")]
    pub stop_level_points: u32,
    /// Distance inside which the terminal freezes an open position, in points.
    #[serde(rename = "freezeLevelPoints")]
    pub freeze_level_points: u32,
    /// Minimum order volume, in lots.
    #[serde(rename = "lotMin")]
    pub lot_min: f64,
    /// Maximum order volume, in lots.
    #[serde(rename = "lotMax")]
    pub lot_max: f64,
    /// Volume increment, in lots.
    #[serde(rename = "lotStep")]
    pub lot_step: f64,
    /// Value of one tick for one lot, in account currency.
    #[serde(rename = "tickValue")]
    pub tick_value: f64,
    /// Size of one tick in price terms.
    #[serde(rename = "tickSize")]
    pub tick_size: f64,
    /// Margin required to open one lot, in account currency.
    #[serde(rename = "marginRequired")]
    pub margin_required: f64,
    /// Swap charged or credited for a long position, per lot.
    #[serde(rename = "swapLong")]
    pub swap_long: f64,
    /// Swap charged or credited for a short position, per lot.
    #[serde(rename = "swapShort")]
    pub swap_short: f64,
    /// MT4 swap accounting mode (0 points, 1 base currency, 2 interest,
    /// 3 margin currency).
    #[serde(rename = "swapType")]
    pub swap_type: u32,
    /// Whether the broker currently allows trading this instrument.
    #[serde(rename = "tradeAllowed")]
    pub trade_allowed: bool,
}

impl SymbolSpecPayload {
    /// Largest accepted price-digit count; anything above is a parse error
    /// rather than a contract decision.
    const MAX_DIGITS: u32 = 10;

    /// Margin the venue would require to open `lots`, in account currency.
    /// Pure arithmetic so callers gate on the same number the terminal uses.
    pub fn margin_for(&self, lots: f64) -> f64 {
        self.margin_required * lots
    }

    /// Rejects unusable contract data before it reaches decision code.
    pub(crate) fn validate(&self) -> Result<(), String> {
        Symbol::parse(&self.symbol)
            .map_err(|error| format!("symbol spec symbol is invalid: {error}"))?;
        if self.digits > Self::MAX_DIGITS {
            return Err(format!("digits must be at most {}", Self::MAX_DIGITS));
        }
        for (name, value) in [("point", self.point), ("tickSize", self.tick_size)] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("{name} must be a finite, positive number"));
            }
        }
        for (name, value) in [
            ("lotMin", self.lot_min),
            ("lotMax", self.lot_max),
            ("lotStep", self.lot_step),
            ("marginRequired", self.margin_required),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("{name} must be a finite, positive number"));
            }
        }
        if self.lot_min > self.lot_max {
            return Err("lotMin must not exceed lotMax".to_owned());
        }
        if self.lot_step > self.lot_max {
            return Err("lotStep must not exceed lotMax".to_owned());
        }
        if !self.tick_value.is_finite() || self.tick_value < 0.0 {
            return Err("tickValue must be a finite, non-negative number".to_owned());
        }
        for (name, value) in [("swapLong", self.swap_long), ("swapShort", self.swap_short)] {
            if !value.is_finite() {
                return Err(format!("{name} must be a finite number"));
            }
        }
        if self.swap_type > 3 {
            return Err("swapType must be 0, 1, 2, or 3".to_owned());
        }
        Ok(())
    }
}

/// Orders-history request sent to the EA: closed orders from the account
/// history, newest first. Realized fills are the only honest source for
/// success rate — floating snapshots miss the exit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderHistoryRequest {
    days: u32,
    magic: u32,
}

impl OrderHistoryRequest {
    /// Default lookback window.
    pub const DEFAULT_DAYS: u32 = 30;
    /// Largest lookback window accepted.
    pub const MAX_DAYS: u32 = 365;

    /// Builds a validated request for `days` of history for one magic number.
    ///
    /// # Errors
    /// Returns [`BrokerError::InvalidPayload`] when the window is zero or
    /// larger than [`Self::MAX_DAYS`].
    pub fn new(days: u32, magic: u32) -> Result<Self, BrokerError> {
        if days == 0 || days > Self::MAX_DAYS {
            return Err(BrokerError::InvalidPayload {
                field: "days",
                reason: "must be from 1 through 365",
            });
        }
        Ok(Self { days, magic })
    }

    /// Lookback window in days.
    pub fn days(&self) -> u32 {
        self.days
    }

    /// Magic number the terminal filters by.
    pub fn magic(&self) -> u32 {
        self.magic
    }
}

/// One closed order as the terminal's account history reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClosedTradePayload {
    /// Venue ticket.
    pub ticket: i64,
    /// Instrument.
    pub symbol: String,
    /// Executed side.
    pub kind: PositionKind,
    /// Volume in lots.
    pub lots: f64,
    /// Entry fill.
    #[serde(rename = "openPrice")]
    pub open_price: f64,
    /// Exit fill.
    #[serde(rename = "closePrice")]
    pub close_price: f64,
    /// Entry time (broker server seconds).
    #[serde(rename = "openTime")]
    pub open_time: i64,
    /// Exit time (broker server seconds).
    #[serde(rename = "closeTime")]
    pub close_time: i64,
    /// Realized gross profit in account currency.
    pub profit: f64,
    /// Swap charged or credited over the hold.
    pub swap: f64,
    /// Commission charged on the round trip.
    pub commission: f64,
    /// Magic number stamped on the order.
    pub magic: u32,
}

impl ClosedTradePayload {
    /// Realized profit after swap and commission, in account currency.
    pub fn net_profit(&self) -> f64 {
        self.profit + self.swap + self.commission
    }

    /// Rejects unusable fills before they reach performance math.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.ticket <= 0 {
            return Err("closed trade ticket must be positive".to_owned());
        }
        Symbol::parse(&self.symbol)
            .map_err(|error| format!("closed trade symbol is invalid: {error}"))?;
        if !matches!(self.kind, PositionKind::Buy | PositionKind::Sell) {
            return Err("closed trade kind must be buy or sell".to_owned());
        }
        if !self.lots.is_finite() || self.lots <= 0.0 {
            return Err("closed trade lots must be a finite, positive number".to_owned());
        }
        for (name, value) in [
            ("openPrice", self.open_price),
            ("closePrice", self.close_price),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("{name} must be a finite, positive price"));
            }
        }
        if self.open_time <= 0 || self.close_time < self.open_time {
            return Err("closed trade times must satisfy 0 < openTime <= closeTime".to_owned());
        }
        for (name, value) in [
            ("profit", self.profit),
            ("swap", self.swap),
            ("commission", self.commission),
        ] {
            if !value.is_finite() {
                return Err(format!("{name} must be a finite number"));
            }
        }
        Ok(())
    }
}

/// Result of an `order_history` command: the closed orders that matched the
/// request, newest first, with the terminal's own truncation report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderHistoryPayload {
    /// Closed orders, newest first.
    pub orders: Vec<ClosedTradePayload>,
    /// Matching orders the terminal held before its response cap.
    pub total: u32,
    /// Whether the terminal omitted orders beyond its cap.
    pub truncated: bool,
}

impl OrderHistoryPayload {
    /// Largest order list the payload accepts.
    pub const MAX_ORDERS: usize = 256;

    /// Rejects unusable history before it reaches performance math.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.orders.len() > Self::MAX_ORDERS {
            return Err(format!(
                "orders must contain at most {} entries",
                Self::MAX_ORDERS
            ));
        }
        if (self.total as usize) < self.orders.len() {
            return Err("total must not be below the returned order count".to_owned());
        }
        for order in &self.orders {
            order.validate()?;
        }
        Ok(())
    }
}

/// Order request sent to the EA for validation or execution, derived only from
/// an approved intent. Fields mirror the intent wire contract so the EA can
/// read them without a nested parser.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderRequest {
    symbol: String,
    side: &'static str,
    order_type: &'static str,
    magic: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    price: Option<f64>,
    volume: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_loss: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    take_profit: Option<f64>,
}

impl OrderRequest {
    /// Maps an approved intent to the EA wire request.
    ///
    /// The intent type can only be minted by the risk gate, so an order
    /// request cannot be built from a raw draft.
    pub fn from_intent(intent: &TradeIntent) -> Self {
        let draft = intent.draft();
        Self {
            symbol: draft.symbol().as_str().to_owned(),
            side: draft.side().as_str(),
            order_type: draft.order().as_str(),
            magic: ORDER_MAGIC,
            price: draft.order().price().map(|price| price.value()),
            volume: draft.volume().value(),
            stop_loss: draft.stop_loss().map(|price| price.value()),
            take_profit: draft.take_profit().map(|price| price.value()),
        }
    }
}

/// Typed result of a completed command.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandPayload {
    /// `ping` carries no payload.
    Ping,
    /// Result of `account_snapshot`.
    AccountSnapshot(AccountSnapshotPayload),
    /// Result of `order_check`; never an executed order.
    OrderCheck(OrderCheckPayload),
    /// Result of `open_order`; reports whether anything reached the broker.
    OpenOrder(OrderExecutionPayload),
    /// Result of `close_order`; reports whether anything reached the broker.
    CloseOrder(OrderExecutionPayload),
    /// Result of `modify_order`; reports whether the stops were changed.
    ModifyOrder(OrderExecutionPayload),
    /// Result of `rates`; the requested window of closed candles.
    Rates(RatesPayload),
    /// Result of `symbol_spec`; the venue contract for one instrument.
    SymbolSpec(SymbolSpecPayload),
    /// Result of `order_history`; realized fills from the account history.
    OrderHistory(OrderHistoryPayload),
}

/// Lifecycle state of one command.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandState {
    /// Delivered (or awaiting delivery) and not yet acknowledged.
    Pending,
    /// Acknowledged successfully with a validated payload.
    Completed {
        /// Validated result payload.
        payload: CommandPayload,
    },
    /// Timed out or acknowledged as failed.
    Failed {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// One command as seen by callers and tests.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandRecord {
    /// Identifier.
    pub id: CommandId,
    /// Requested kind.
    pub kind: CommandKind,
    /// Current state.
    pub state: CommandState,
}

/// One command as listed for operators: identity, lifecycle, and a bounded
/// result summary that never carries raw account balances.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedCommand {
    /// Identifier.
    pub id: CommandId,
    /// Requested kind.
    pub kind: CommandKind,
    /// Lifecycle status: pending, completed, or failed.
    pub status: &'static str,
    /// Bounded result summary for completed commands.
    pub summary: Option<Value>,
    /// Non-sensitive failure reason for failed commands.
    pub reason: Option<String>,
}
