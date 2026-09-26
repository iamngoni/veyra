//! Validated trade-intent values and their JSON contract.
//!
//! A [`TradeIntentDraft`] is the only shape a strategy or model may propose.
//! Drafts are parsed once at the boundary into symbols, sides, order kinds,
//! volumes, prices, and comments that cannot represent invalid values. Drafts
//! deliberately carry no identity; the risk gate mints a [`TradeIntent`] only
//! when it approves one, so execution code can never reference something that
//! failed validation.

use std::fmt;

use serde::de::{Deserializer, Error as DeError};
use serde::{Deserialize, Serialize, Serializer};
use uuid::Uuid;

use crate::broker::{BrokerError, Symbol};

/// Largest order size the parser accepts; venue steps and policy caps are
/// enforced later, not here.
const MAX_VOLUME_LOTS: f64 = 100.0;
/// Largest instrument price the parser accepts.
const MAX_PRICE: f64 = 10_000_000.0;
/// MT4 truncates longer order comments, so the parser rejects them instead.
const MAX_COMMENT_CHARS: usize = 31;

/// Errors raised while parsing an intent, a draft, or a model proposal.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid `{field}`: {reason}")]
pub struct IntentError {
    /// Wire field that failed validation.
    pub field: &'static str,
    /// Non-sensitive acceptance rule.
    pub reason: &'static str,
}

impl IntentError {
    fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

impl From<BrokerError> for IntentError {
    fn from(error: BrokerError) -> Self {
        match error {
            BrokerError::InvalidPayload { field, reason } => Self::new(field, reason),
            BrokerError::Construction { .. } => {
                Self::new("symbol", "must be a valid instrument symbol")
            }
        }
    }
}

/// Direction of a requested order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Long position.
    Buy,
    /// Short position.
    Sell,
}

impl Side {
    /// Returns the wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }

    fn parse(value: &str) -> Result<Self, IntentError> {
        match value {
            "buy" => Ok(Self::Buy),
            "sell" => Ok(Self::Sell),
            _ => Err(IntentError::new("side", "must be `buy` or `sell`")),
        }
    }
}

/// A positive, finite price. No deserializer can bypass the constructor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price(f64);

impl Price {
    /// Parses a finite price greater than 0 and at most 10,000,000.
    pub fn parse(value: f64) -> Result<Self, IntentError> {
        if value.is_finite() && value > 0.0 && value <= MAX_PRICE {
            Ok(Self(value))
        } else {
            Err(IntentError::new(
                "price",
                "must be a finite number greater than 0 and at most 10000000",
            ))
        }
    }

    /// Returns the accepted price.
    pub fn value(self) -> f64 {
        self.0
    }
}

/// A positive, finite lot volume.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume(f64);

impl Volume {
    /// The smallest standard lot step; also the restrictive policy default.
    pub const MINIMUM: Self = Self(0.01);

    /// Parses a finite volume greater than 0 and at most 100 lots.
    pub fn parse(value: f64) -> Result<Self, IntentError> {
        if value.is_finite() && value > 0.0 && value <= MAX_VOLUME_LOTS {
            Ok(Self(value))
        } else {
            Err(IntentError::new(
                "volume",
                "must be a finite number greater than 0 and at most 100",
            ))
        }
    }

    /// Returns the accepted volume.
    pub fn value(self) -> f64 {
        self.0
    }
}

/// An order comment broker terminals accept: 1-31 printable ASCII characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment(String);

impl Comment {
    /// Parses a non-empty comment within the length MT4 keeps intact.
    pub fn parse(value: &str) -> Result<Self, IntentError> {
        let valid = !value.is_empty()
            && value.len() <= MAX_COMMENT_CHARS
            && value.chars().all(|c| c.is_ascii_graphic() || c == ' ');
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(IntentError::new(
                "comment",
                "must be 1-31 printable ASCII characters",
            ))
        }
    }

    /// Returns the accepted comment.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Requested execution style. Limit and stop orders carry their trigger price
/// inside the variant, so a price can never be missing or attached to a market
/// order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrderKind {
    /// Execute at the current market price; no trigger price.
    Market,
    /// Execute when the market reaches `price` or better.
    Limit(Price),
    /// Execute when the market reaches `price` or worse.
    Stop(Price),
}

impl OrderKind {
    /// Returns the wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Market => "market",
            Self::Limit(_) => "limit",
            Self::Stop(_) => "stop",
        }
    }

    /// Returns the trigger price, if the order needs one.
    pub fn price(self) -> Option<Price> {
        match self {
            Self::Market => None,
            Self::Limit(price) | Self::Stop(price) => Some(price),
        }
    }
}

/// Parses an instrument, trimmed, keeping its spelling: broker names such as
/// `SP500m` are mixed-case and must reach the terminal as written. Symbol
/// equality ignores case, so allowlists and intents still match; an approved
/// intent takes the allowlist's spelling (see [`crate::risk::RiskGate`]).
pub fn parse_instrument(raw: &str) -> Result<Symbol, IntentError> {
    Symbol::parse(raw.trim()).map_err(IntentError::from)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTradeIntent {
    symbol: String,
    side: String,
    order_type: String,
    #[serde(default)]
    price: Option<f64>,
    volume: f64,
    #[serde(default)]
    stop_loss: Option<f64>,
    #[serde(default)]
    take_profit: Option<f64>,
    #[serde(default)]
    comment: Option<String>,
}

/// One proposed order, validated.
///
/// It carries no identity: the risk gate mints a [`TradeIntent`] only when it
/// approves a draft.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "RawTradeIntent")]
pub struct TradeIntentDraft {
    symbol: Symbol,
    side: Side,
    order: OrderKind,
    volume: Volume,
    stop_loss: Option<Price>,
    take_profit: Option<Price>,
    comment: Option<Comment>,
}

impl TradeIntentDraft {
    /// The same draft under another spelling of its symbol (same instrument,
    /// by case-insensitive equality). Used to send the allowlist's spelling.
    pub(crate) fn with_symbol_spelling(&self, symbol: Symbol) -> Self {
        Self {
            symbol,
            ..self.clone()
        }
    }

    /// Builds a draft from already validated components.
    pub fn new(
        symbol: Symbol,
        side: Side,
        order: OrderKind,
        volume: Volume,
        stop_loss: Option<Price>,
        take_profit: Option<Price>,
        comment: Option<Comment>,
    ) -> Self {
        Self {
            symbol,
            side,
            order,
            volume,
            stop_loss,
            take_profit,
            comment,
        }
    }

    /// Instrument the intent targets.
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    /// Direction of the intent.
    pub fn side(&self) -> Side {
        self.side
    }

    /// Requested execution style and trigger price, if any.
    pub fn order(&self) -> OrderKind {
        self.order
    }

    /// Requested lot volume.
    pub fn volume(&self) -> Volume {
        self.volume
    }

    /// Protective stop price, if any.
    pub fn stop_loss(&self) -> Option<Price> {
        self.stop_loss
    }

    /// Take-profit price, if any.
    pub fn take_profit(&self) -> Option<Price> {
        self.take_profit
    }

    /// Broker-visible comment, if any.
    pub fn comment(&self) -> Option<&Comment> {
        self.comment.as_ref()
    }
}

fn price_for(field: &'static str, value: f64) -> Result<Price, IntentError> {
    Price::parse(value).map_err(|error| IntentError::new(field, error.reason))
}

impl TryFrom<RawTradeIntent> for TradeIntentDraft {
    type Error = IntentError;

    fn try_from(raw: RawTradeIntent) -> Result<Self, Self::Error> {
        let symbol = parse_instrument(&raw.symbol)?;
        let side = Side::parse(&raw.side)?;
        let volume = Volume::parse(raw.volume)?;
        let order = match raw.order_type.as_str() {
            "market" => {
                if raw.price.is_some() {
                    return Err(IntentError::new(
                        "price",
                        "must be absent for market orders",
                    ));
                }
                OrderKind::Market
            }
            "limit" | "stop" => {
                let price = raw.price.ok_or_else(|| {
                    IntentError::new("price", "is required for limit and stop orders")
                })?;
                let price = price_for("price", price)?;
                if raw.order_type == "limit" {
                    OrderKind::Limit(price)
                } else {
                    OrderKind::Stop(price)
                }
            }
            _ => {
                return Err(IntentError::new(
                    "order_type",
                    "must be `market`, `limit`, or `stop`",
                ));
            }
        };
        let stop_loss = raw
            .stop_loss
            .map(|value| price_for("stop_loss", value))
            .transpose()?;
        let take_profit = raw
            .take_profit
            .map(|value| price_for("take_profit", value))
            .transpose()?;
        let comment = raw
            .comment
            .map(|value| Comment::parse(&value))
            .transpose()?;
        Ok(Self::new(
            symbol,
            side,
            order,
            volume,
            stop_loss,
            take_profit,
            comment,
        ))
    }
}

#[derive(Serialize)]
struct RawTradeIntentOut<'a> {
    symbol: &'a str,
    side: &'static str,
    order_type: &'static str,
    price: Option<f64>,
    volume: f64,
    stop_loss: Option<f64>,
    take_profit: Option<f64>,
    comment: Option<&'a str>,
}

impl Serialize for TradeIntentDraft {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        RawTradeIntentOut {
            symbol: self.symbol.as_str(),
            side: self.side.as_str(),
            order_type: self.order.as_str(),
            price: self.order.price().map(Price::value),
            volume: self.volume.value(),
            stop_loss: self.stop_loss.map(Price::value),
            take_profit: self.take_profit.map(Price::value),
            comment: self.comment.as_ref().map(Comment::as_str),
        }
        .serialize(serializer)
    }
}

/// Identity minted when the risk gate approves a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentId(Uuid);

impl IntentId {
    /// Generates a new random identity.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for IntentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for IntentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl Serialize for IntentId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

/// An approved intent: the only value execution code may reference.
#[derive(Debug, Clone, PartialEq)]
pub struct TradeIntent {
    id: IntentId,
    draft: TradeIntentDraft,
}

impl TradeIntent {
    /// Mints an identity for an approved draft.
    ///
    /// Only the risk gate calls this; the crate-private constructor keeps
    /// external callers from declaring a draft approved without an evaluation.
    pub(crate) fn approve(draft: TradeIntentDraft) -> Self {
        Self {
            id: IntentId::new(),
            draft,
        }
    }

    /// Returns the approved intent's identity.
    pub fn id(&self) -> IntentId {
        self.id
    }

    /// Returns the approved draft.
    pub fn draft(&self) -> &TradeIntentDraft {
        &self.draft
    }
}

impl Serialize for TradeIntent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct IntentOut<'a> {
            id: IntentId,
            #[serde(flatten)]
            draft: &'a TradeIntentDraft,
        }
        IntentOut {
            id: self.id,
            draft: &self.draft,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProposal {
    action: String,
    #[serde(default)]
    intent: Option<RawTradeIntent>,
}

/// A model answer parsed into the only two legal shapes: no trade, or one
/// validated draft. Unknown actions, unknown fields, and mismatched shapes are
/// rejected.
#[derive(Debug, Clone, PartialEq)]
pub enum TradeProposal {
    /// The model declined to trade.
    None,
    /// The model proposed one order.
    Open(TradeIntentDraft),
}

impl<'de> Deserialize<'de> for TradeProposal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawProposal::deserialize(deserializer)?;
        match raw.action.as_str() {
            "none" => match raw.intent {
                None => Ok(Self::None),
                Some(_) => Err(DeError::custom(IntentError::new(
                    "intent",
                    "must be absent when action is `none`",
                ))),
            },
            "open" => match raw.intent {
                Some(intent) => Ok(Self::Open(
                    TradeIntentDraft::try_from(intent).map_err(DeError::custom)?,
                )),
                None => Err(DeError::custom(IntentError::new(
                    "intent",
                    "is required when action is `open`",
                ))),
            },
            _ => Err(DeError::custom(IntentError::new(
                "action",
                "must be `none` or `open`",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn draft() -> TradeIntentDraft {
        TradeIntentDraft::new(
            parse_instrument("EURUSD").expect("symbol"),
            Side::Buy,
            OrderKind::Market,
            Volume::parse(0.01).expect("volume"),
            None,
            None,
            None,
        )
    }

    #[test]
    fn market_draft_parses_and_round_trips() {
        let parsed: TradeIntentDraft = serde_json::from_value(json!({
            "symbol": "eurusd",
            "side": "buy",
            "order_type": "market",
            "volume": 0.01,
            "stop_loss": 1.05,
            "comment": "v1"
        }))
        .expect("valid draft");

        assert_eq!(parsed.symbol().as_str(), "eurusd", "spelling is kept");
        assert_eq!(parsed.side(), Side::Buy);
        assert_eq!(parsed.order(), OrderKind::Market);
        assert_eq!(parsed.volume().value(), 0.01);
        assert_eq!(parsed.stop_loss().map(Price::value), Some(1.05));
        assert_eq!(parsed.take_profit(), None);
        assert_eq!(parsed.comment().map(Comment::as_str), Some("v1"));

        let wire = serde_json::to_value(&parsed).expect("serializable");
        assert_eq!(
            wire,
            json!({
                "symbol": "eurusd",
                "side": "buy",
                "order_type": "market",
                "price": null,
                "volume": 0.01,
                "stop_loss": 1.05,
                "take_profit": null,
                "comment": "v1"
            })
        );
    }

    #[test]
    fn limit_and_stop_orders_require_their_trigger_price() {
        let limit: TradeIntentDraft = serde_json::from_value(json!({
            "symbol": "EURUSD",
            "side": "sell",
            "order_type": "limit",
            "price": 1.2,
            "volume": 0.02
        }))
        .expect("valid limit draft");
        assert_eq!(
            limit.order(),
            OrderKind::Limit(Price::parse(1.2).expect("price"))
        );

        let stop: TradeIntentDraft = serde_json::from_value(json!({
            "symbol": "EURUSD",
            "side": "sell",
            "order_type": "stop",
            "price": 1.2,
            "volume": 0.02
        }))
        .expect("valid stop draft");
        assert_eq!(
            stop.order(),
            OrderKind::Stop(Price::parse(1.2).expect("price"))
        );

        let missing: Result<TradeIntentDraft, _> = serde_json::from_value(json!({
            "symbol": "EURUSD",
            "side": "buy",
            "order_type": "limit",
            "volume": 0.02
        }));
        let error = missing.expect_err("price is required");
        assert!(error.to_string().contains("price"));
    }

    #[test]
    fn market_orders_reject_a_trigger_price() {
        let error: IntentError = TradeIntentDraft::try_from(RawTradeIntent {
            symbol: "EURUSD".to_owned(),
            side: "buy".to_owned(),
            order_type: "market".to_owned(),
            price: Some(1.1),
            volume: 0.01,
            stop_loss: None,
            take_profit: None,
            comment: None,
        })
        .expect_err("market orders carry no price");
        assert_eq!(error.field, "price");
    }

    #[test]
    fn unknown_fields_and_actions_are_rejected() {
        let unknown: Result<TradeIntentDraft, _> = serde_json::from_value(json!({
            "symbol": "EURUSD",
            "side": "buy",
            "order_type": "market",
            "volume": 0.01,
            "leverage": 100
        }));
        assert!(unknown.is_err());

        let action: Result<TradeProposal, _> = serde_json::from_value(json!({"action": "close"}));
        assert!(action.is_err());
    }

    #[test]
    fn invalid_values_name_the_failing_field() {
        let message = |extra: serde_json::Value| {
            let mut base = json!({
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "market",
                "volume": 0.01
            });
            let object = base.as_object_mut().expect("object");
            for (key, entry) in extra.as_object().expect("object") {
                object.insert(key.clone(), entry.clone());
            }
            serde_json::from_value::<TradeIntentDraft>(base)
                .expect_err("must be rejected")
                .to_string()
        };

        assert!(message(json!({"volume": -1.0})).contains("volume"));
        assert!(message(json!({"symbol": "not a symbol"})).contains("symbol"));
        assert!(message(json!({"side": "long"})).contains("side"));
        assert!(message(json!({"order_type": "iceberg"})).contains("order_type"));
        assert!(message(json!({"stop_loss": 0.0})).contains("stop_loss"));
        assert!(message(json!({"take_profit": -5.0})).contains("take_profit"));
        assert!(message(json!({"comment": "x".repeat(32)})).contains("comment"));
        assert!(message(json!({"comment": "two\nlines"})).contains("comment"));
    }

    #[test]
    fn non_finite_numbers_are_rejected_before_any_calculation() {
        assert!(Volume::parse(f64::NAN).is_err());
        assert!(Volume::parse(f64::INFINITY).is_err());
        assert!(Volume::parse(100.01).is_err());
        assert!(Price::parse(f64::NAN).is_err());
        assert!(Price::parse(0.0).is_err());
        assert_eq!(Volume::MINIMUM.value(), 0.01);
    }

    #[test]
    fn proposals_parse_only_legal_shapes() {
        assert_eq!(
            serde_json::from_value::<TradeProposal>(json!({"action": "none"})).expect("none"),
            TradeProposal::None
        );

        let open = serde_json::from_value::<TradeProposal>(json!({
            "action": "open",
            "intent": {"symbol": "EURUSD", "side": "sell", "order_type": "market", "volume": 0.05}
        }))
        .expect("open");
        assert!(matches!(open, TradeProposal::Open(_)));

        let open_without_intent: Result<TradeProposal, _> =
            serde_json::from_value(json!({"action": "open"}));
        assert!(open_without_intent.is_err());

        let none_with_intent: Result<TradeProposal, _> = serde_json::from_value(json!({
            "action": "none",
            "intent": {"symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": 0.01}
        }));
        assert!(none_with_intent.is_err());

        let unknown_field: Result<TradeProposal, _> =
            serde_json::from_value(json!({"action": "none", "confidence": 0.9}));
        assert!(unknown_field.is_err());
    }

    #[test]
    fn approved_intents_carry_an_identity_and_the_draft() {
        let intent = TradeIntent::approve(draft());
        assert_eq!(intent.draft().symbol().as_str(), "EURUSD");
        assert_eq!(intent.id().to_string().len(), 36);

        let wire = serde_json::to_value(&intent).expect("serializable");
        assert_eq!(wire["symbol"], "EURUSD");
        assert_eq!(wire["side"], "buy");
        assert!(wire["id"].as_str().is_some());
        assert_eq!(wire["volume"], 0.01);
    }

    #[test]
    fn instrument_parsing_trims_and_keeps_the_broker_spelling() {
        assert_eq!(
            parse_instrument("  SP500m ").expect("valid").as_str(),
            "SP500m",
            "mixed-case broker names reach the terminal as written"
        );
        assert_eq!(
            parse_instrument("eurusd").expect("valid"),
            parse_instrument("EURUSD").expect("valid"),
            "but the same instrument in any case is equal"
        );
        assert_eq!(parse_instrument("").expect_err("empty").field, "symbol");
    }
}
