//! The deterministic risk gate.
//!
//! Fixed check order: kill switch, instrument allowlist, session window,
//! per-order volume cap, account availability, trading permission, open-order
//! cap, duplicate suppression. Every check is a pure function of the policy,
//! the draft, the supplied account facts, and the supplied clock; the gate
//! never fetches state itself, so evaluation is deterministic and testable.
//!
//! Missing or stale account state rejects instead of guessing, and only an
//! approved draft is minted into a [`TradeIntent`].

use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use super::RiskPolicy;
use super::valuation::{self, PositionFact};
use crate::broker::Symbol;
use crate::trading::intent::{TradeIntent, TradeIntentDraft};

/// How many approvals are remembered for duplicate suppression.
const APPROVAL_HISTORY: usize = 128;
/// Slack allowed when comparing summed lot volumes.
const EXPOSURE_EPSILON: f64 = 1e-9;

/// Facts the gate needs from the venue, assembled by the caller from a fresh
/// link report. Nothing here is inferred: when the caller cannot supply fresh
/// facts, it passes `None` and the gate rejects.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountFacts {
    /// The terminal currently allows trading operations.
    pub trade_allowed: bool,
    /// Open venue orders (MT4 counts positions and pending orders here).
    pub open_orders: u32,
    /// Total open volume across every open order, in lots.
    pub open_lots: f64,
    /// Symbols carrying an open venue order (any magic), from the latest
    /// validated snapshot. Used to enforce one position per asset.
    pub open_symbols: Vec<Symbol>,
    /// Open positions with side and volume, for net-exposure checks.
    pub open_positions: Vec<PositionFact>,
    /// Reference prices the caller can vouch for (venue snapshot `current`
    /// values plus any market data it holds), used to value draft risk.
    pub prices: Vec<(Symbol, f64)>,
    /// Account equity reported by the latest validated snapshot, when known.
    pub equity: Option<f64>,
    /// Percentage below the current UTC day's opening equity, when tracked.
    pub day_drawdown_percent: Option<f64>,
    /// Percentage below the highest equity since startup, when tracked.
    pub peak_drawdown_percent: Option<f64>,
}

/// Stable rejection codes; additions are backwards-compatible for consumers
/// that match on the string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskCode {
    /// The operator engaged the kill switch.
    KillSwitch,
    /// The instrument is not on the configured allowlist.
    SymbolNotAllowed,
    /// The current UTC hour is outside the configured session window.
    SessionClosed,
    /// The requested volume exceeds the per-order cap.
    VolumeAboveLimit,
    /// No fresh account state was available; the gate fails closed.
    AccountStateUnavailable,
    /// The terminal reports that trading is not allowed.
    TradingNotAllowed,
    /// The venue already holds the maximum tolerated number of orders.
    OrderLimitReached,
    /// Open volume plus the requested volume exceeds the total exposure cap.
    ExposureAboveLimit,
    /// An identical draft was approved inside the duplicate window.
    DuplicateIntent,
    /// A position is already open on the requested instrument.
    SymbolAlreadyOpen,
    /// The built-in entry window (rollover blackout or weekend guard) is closed.
    MarketWindowClosed,
    /// The draft's stop risk exceeds the configured per-trade cap.
    RiskAboveLimit,
    /// A draft price is known but the risk cannot be valued (unknown pair).
    RiskUnverifiable,
    /// Equity is below the daily-loss breaker.
    DailyLossLimitReached,
    /// Equity is below the peak-drawdown breaker.
    PeakDrawdownLimitReached,
    /// The net directional exposure would exceed the factor cap.
    FactorExposureAboveLimit,
}

impl RiskCode {
    /// Returns the stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KillSwitch => "kill_switch",
            Self::SymbolNotAllowed => "symbol_not_allowed",
            Self::SessionClosed => "session_closed",
            Self::VolumeAboveLimit => "volume_above_limit",
            Self::AccountStateUnavailable => "account_state_unavailable",
            Self::TradingNotAllowed => "trading_not_allowed",
            Self::OrderLimitReached => "order_limit_reached",
            Self::ExposureAboveLimit => "exposure_above_limit",
            Self::DuplicateIntent => "duplicate_intent",
            Self::SymbolAlreadyOpen => "symbol_already_open",
            Self::MarketWindowClosed => "market_window_closed",
            Self::RiskAboveLimit => "risk_above_limit",
            Self::RiskUnverifiable => "risk_unverifiable",
            Self::DailyLossLimitReached => "daily_loss_limit",
            Self::PeakDrawdownLimitReached => "peak_drawdown_limit",
            Self::FactorExposureAboveLimit => "factor_exposure_above_limit",
        }
    }

    /// Non-sensitive operator explanation.
    pub fn detail(self) -> &'static str {
        match self {
            Self::KillSwitch => "the kill switch is engaged",
            Self::SymbolNotAllowed => "the instrument is not on the allowlist",
            Self::SessionClosed => "the current UTC hour is outside the session window",
            Self::VolumeAboveLimit => "the requested volume exceeds the per-order cap",
            Self::AccountStateUnavailable => "no fresh account state is available",
            Self::SymbolAlreadyOpen => {
                "a position is already open on this instrument (one position per asset)"
            }
            Self::MarketWindowClosed => {
                "the entry window is closed (rollover blackout or weekend guard)"
            }
            Self::RiskAboveLimit => "the stop would risk more than the per-trade cap allows",
            Self::RiskUnverifiable => {
                "the draft price is known but the instrument cannot be valued"
            }
            Self::DailyLossLimitReached => "equity is below the daily-loss breaker",
            Self::PeakDrawdownLimitReached => "equity is below the peak-drawdown breaker",
            Self::FactorExposureAboveLimit => {
                "net directional exposure would exceed the factor cap"
            }
            Self::TradingNotAllowed => "the terminal reports trading is not allowed",
            Self::OrderLimitReached => "the venue already holds the maximum tolerated orders",
            Self::ExposureAboveLimit => {
                "open exposure plus the requested volume exceeds the total cap"
            }
            Self::DuplicateIntent => "an identical intent was approved inside the duplicate window",
        }
    }
}

/// One deterministic rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskRejection {
    code: RiskCode,
}

impl RiskRejection {
    /// Returns the rejection code.
    pub fn code(self) -> RiskCode {
        self.code
    }

    /// Returns the non-sensitive explanation.
    pub fn detail(self) -> &'static str {
        self.code.detail()
    }
}

/// Result of one gate evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum RiskDecision {
    /// The draft satisfied every control and now carries an identity.
    Approved(TradeIntent),
    /// The draft failed a control; nothing may execute.
    Rejected(RiskRejection),
}

impl Serialize for RiskDecision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Approved(intent) => {
                let mut state = serializer.serialize_struct("decision", 2)?;
                state.serialize_field("decision", "approved")?;
                state.serialize_field("intent", intent)?;
                state.end()
            }
            Self::Rejected(rejection) => {
                let mut state = serializer.serialize_struct("decision", 3)?;
                state.serialize_field("decision", "rejected")?;
                state.serialize_field("code", rejection.code.as_str())?;
                state.serialize_field("detail", rejection.detail())?;
                state.end()
            }
        }
    }
}

/// One remembered approval; draft equality is what suppresses duplicates.
#[derive(Debug, Clone)]
struct Approval {
    draft: TradeIntentDraft,
    at: SystemTime,
}

/// Deterministic, fail-closed intent gate. Clones share the duplicate memory.
#[derive(Debug, Clone)]
pub struct RiskGate {
    policy: Arc<RwLock<RiskPolicy>>,
    approvals: Arc<Mutex<Vec<Approval>>>,
}

impl RiskGate {
    /// Builds a gate with an empty approval memory.
    pub fn new(policy: RiskPolicy) -> Self {
        Self {
            policy: Arc::new(RwLock::new(policy)),
            approvals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns a snapshot of the active policy.
    ///
    /// The policy can be replaced at runtime from the control surface; every
    /// decision reads one consistent snapshot rather than a live reference.
    pub fn policy(&self) -> RiskPolicy {
        self.read().clone()
    }

    /// Replaces the active policy. Callers validate the new policy first;
    /// the write is atomic for every concurrent decision.
    pub fn update_policy(&self, policy: RiskPolicy) {
        *self.write() = policy;
    }

    /// Reads the policy lock, recovering a poisoned guard the same way the
    /// approval memory does: whole values are written, so recovery is safe.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, RiskPolicy> {
        match self.policy.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, RiskPolicy> {
        match self.policy.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Evaluates one draft in the documented check order.
    pub fn evaluate(
        &self,
        draft: &TradeIntentDraft,
        account: Option<AccountFacts>,
        now: SystemTime,
    ) -> RiskDecision {
        match self.check(draft, account, now) {
            Err(code) => Self::reject(code),
            Ok(()) => {
                if self.suppress_duplicate(draft, now) {
                    return Self::reject(RiskCode::DuplicateIntent);
                }
                RiskDecision::Approved(TradeIntent::approve(draft.clone()))
            }
        }
    }

    /// Non-recording dry run for the agent loop: every deterministic rule
    /// applies, but the duplicate memory is neither read nor written, so a
    /// preview can never consume the single approval a draft gets.
    pub fn preview(
        &self,
        draft: &TradeIntentDraft,
        account: Option<AccountFacts>,
        now: SystemTime,
    ) -> RiskDecision {
        match self.check(draft, account, now) {
            Err(code) => Self::reject(code),
            Ok(()) => RiskDecision::Approved(TradeIntent::approve(draft.clone())),
        }
    }

    /// The stateless half of [`RiskGate::evaluate`]: every rule except the
    /// duplicate window.
    fn check(
        &self,
        draft: &TradeIntentDraft,
        account: Option<AccountFacts>,
        now: SystemTime,
    ) -> Result<(), RiskCode> {
        let policy = self.policy();
        if policy.kill_switch() {
            return Err(RiskCode::KillSwitch);
        }
        if !policy.allows_symbol(draft.symbol()) {
            return Err(RiskCode::SymbolNotAllowed);
        }
        // Built-in calendar guards: rollover blackout and the weekend cutoff.
        // They run for every venue and cannot be disabled by an empty session.
        if crate::risk::window::entry_block(now, None).is_some() {
            return Err(RiskCode::MarketWindowClosed);
        }
        if let Some(window) = policy.session()
            && !window.contains(utc_hour(now))
        {
            return Err(RiskCode::SessionClosed);
        }
        if draft.volume().value() > policy.max_volume_per_order().value() {
            return Err(RiskCode::VolumeAboveLimit);
        }
        let Some(account) = account else {
            return Err(RiskCode::AccountStateUnavailable);
        };
        if !account.trade_allowed {
            return Err(RiskCode::TradingNotAllowed);
        }
        if policy.max_daily_loss_percent() > 0.0
            && account
                .day_drawdown_percent
                .is_some_and(|value| value >= policy.max_daily_loss_percent())
        {
            return Err(RiskCode::DailyLossLimitReached);
        }
        if policy.max_peak_drawdown_percent() > 0.0
            && account
                .peak_drawdown_percent
                .is_some_and(|value| value >= policy.max_peak_drawdown_percent())
        {
            return Err(RiskCode::PeakDrawdownLimitReached);
        }
        if account
            .open_symbols
            .iter()
            .any(|open| open == draft.symbol())
        {
            return Err(RiskCode::SymbolAlreadyOpen);
        }
        if account.open_orders >= policy.max_open_orders() {
            return Err(RiskCode::OrderLimitReached);
        }
        if account.open_lots + draft.volume().value()
            > policy.max_total_lots().value() + EXPOSURE_EPSILON
        {
            return Err(RiskCode::ExposureAboveLimit);
        }
        if policy.max_risk_percent() > 0.0 && draft.stop_loss().is_some() {
            let reference = draft
                .order()
                .price()
                .map(|price| price.value())
                .or_else(|| {
                    account
                        .prices
                        .iter()
                        .find(|(symbol, _)| symbol == draft.symbol())
                        .map(|(_, price)| *price)
                });
            if let Some(reference) = reference
                && let Some(equity) = account.equity
            {
                match valuation::risk_percent(draft, Some(reference), equity) {
                    Some(risk) if risk > policy.max_risk_percent() + EXPOSURE_EPSILON => {
                        return Err(RiskCode::RiskAboveLimit);
                    }
                    Some(_) => {}
                    None => return Err(RiskCode::RiskUnverifiable),
                }
            }
        }
        if policy.max_net_factor_lots() > 0.0 {
            let net = valuation::net_usd_lots(&account.open_positions, draft);
            if net.abs() > policy.max_net_factor_lots() + EXPOSURE_EPSILON {
                return Err(RiskCode::FactorExposureAboveLimit);
            }
        }
        Ok(())
    }

    fn reject(code: RiskCode) -> RiskDecision {
        RiskDecision::Rejected(RiskRejection { code })
    }

    /// Returns true when an equal draft was approved inside the window, and
    /// otherwise records this one.
    ///
    /// The guard holds a plain vector, so recovering a poisoned lock cannot
    /// observe a half-written state; a lock error therefore degrades to normal
    /// operation instead of blocking trading outright.
    fn suppress_duplicate(&self, draft: &TradeIntentDraft, now: SystemTime) -> bool {
        let window = self.policy().duplicate_window();
        if window.is_zero() {
            return false;
        }
        let mut approvals = match self.approvals.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        approvals.retain(|approval| {
            now.duration_since(approval.at)
                .map_or(true, |age| age <= window)
        });
        if approvals.iter().any(|approval| approval.draft == *draft) {
            return true;
        }
        if approvals.len() == APPROVAL_HISTORY {
            approvals.remove(0);
        }
        approvals.push(Approval {
            draft: draft.clone(),
            at: now,
        });
        false
    }
}

/// UTC hour of an instant; instants before the epoch count as hour 0.
fn utc_hour(now: SystemTime) -> u8 {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    ((seconds / 3_600) % 24) as u8
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::Duration;

    use super::*;
    use crate::broker::Symbol;
    use crate::risk::SessionWindow;
    use crate::trading::intent::{OrderKind, Price, Side, Volume, parse_instrument};

    fn symbol() -> Symbol {
        parse_instrument("EURUSD").expect("symbol")
    }

    fn draft(volume: f64) -> TradeIntentDraft {
        TradeIntentDraft::new(
            symbol(),
            Side::Buy,
            OrderKind::Market,
            Volume::parse(volume).expect("volume"),
            None,
            None,
            None,
        )
    }

    fn policy() -> RiskPolicy {
        RiskPolicy::new(
            false,
            vec![symbol()],
            Volume::parse(0.5).expect("volume"),
            Volume::parse(0.5).expect("volume"),
            2,
            Duration::from_secs(60),
            None,
        )
    }

    fn facts(open_orders: u32) -> Option<AccountFacts> {
        facts_with(open_orders, 0.0)
    }

    fn facts_with(open_orders: u32, open_lots: f64) -> Option<AccountFacts> {
        Some(AccountFacts {
            trade_allowed: true,
            open_orders,
            open_lots,
            open_symbols: Vec::new(),
            equity: Some(1_000.0),
            open_positions: Vec::new(),
            prices: Vec::new(),
            day_drawdown_percent: None,
            peak_drawdown_percent: None,
        })
    }

    #[test]
    fn rollover_and_weekend_windows_reject_entries() {
        let gate = RiskGate::new(policy());
        // Thursday 2026-01-08 21:00 UTC: inside the rollover blackout.
        let rollover = UNIX_EPOCH + Duration::from_secs(1_767_906_000);
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), facts(0), rollover)).code(),
            RiskCode::MarketWindowClosed
        );

        // Friday 2026-01-09 20:00 UTC: past the weekend entry cutoff.
        let friday_evening = UNIX_EPOCH + Duration::from_secs(1_767_988_800);
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), facts(0), friday_evening)).code(),
            RiskCode::MarketWindowClosed
        );

        // Wednesday midday remains open.
        let wednesday = UNIX_EPOCH + Duration::from_secs(1_767_787_200);
        assert!(matches!(
            gate.evaluate(&draft(0.1), facts(0), wednesday),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn a_symbol_with_an_open_order_is_rejected() {
        let gate = RiskGate::new(policy());
        let now = at(10);
        let rejection = expect_rejection(gate.evaluate(&draft(0.1), facts_holding("EURUSD"), now));
        assert_eq!(rejection.code(), RiskCode::SymbolAlreadyOpen);
        assert_eq!(rejection.code().as_str(), "symbol_already_open");

        // A different instrument is unaffected by the one-per-asset rule.
        assert!(matches!(
            gate.evaluate(&draft(0.1), facts_holding("GBPUSD"), now),
            RiskDecision::Approved(_)
        ));
    }

    /// Draft with a stop and an explicit side, for valuation tests.
    fn draft_sided(symbol_name: &str, side: Side, volume: f64, stop: f64) -> TradeIntentDraft {
        TradeIntentDraft::new(
            Symbol::parse(symbol_name).expect("symbol"),
            side,
            OrderKind::Market,
            Volume::parse(volume).expect("volume"),
            Some(Price::parse(stop).expect("stop")),
            None,
            None,
        )
    }

    /// Buy draft with a stop.
    fn draft_with_stop(symbol: &str, volume: f64, stop: f64) -> TradeIntentDraft {
        draft_sided(symbol, Side::Buy, volume, stop)
    }

    /// Baseline facts with a reference price table and equity.
    fn facts_priced(prices: &[(&str, f64)]) -> AccountFacts {
        let mut facts = facts_test(0);
        facts.prices = prices
            .iter()
            .map(|(name, price)| (Symbol::parse(name).expect("symbol"), *price))
            .collect();
        facts.equity = Some(1_000.0);
        facts
    }

    /// Baseline facts with an open position on the book.
    fn facts_positioned(symbol_name: &str, side: Side, lots: f64) -> AccountFacts {
        let mut facts = facts_test(1);
        let symbol = Symbol::parse(symbol_name).expect("symbol");
        facts.open_symbols = vec![symbol.clone()];
        facts.open_positions = vec![PositionFact { symbol, side, lots }];
        facts.open_lots = lots;
        facts
    }

    fn facts_test(open_orders: u32) -> AccountFacts {
        AccountFacts {
            trade_allowed: true,
            open_orders,
            open_lots: 0.0,
            open_symbols: Vec::new(),
            open_positions: Vec::new(),
            prices: Vec::new(),
            equity: Some(1_000.0),
            day_drawdown_percent: None,
            peak_drawdown_percent: None,
        }
    }

    /// Policy with focused limits so one rule can be tested at a time.
    fn rule_policy(risk: f64, daily: f64, peak: f64, factor: f64) -> RiskGate {
        RiskGate::new(policy().with_limits(risk, daily, peak, factor))
    }

    /// Rebuilds a policy with a different allowlist, keeping every limit.
    fn policy_with_symbols(policy: RiskPolicy, symbols: Vec<Symbol>) -> RiskPolicy {
        RiskPolicy::new(
            policy.kill_switch(),
            symbols,
            policy.max_volume_per_order(),
            policy.max_total_lots(),
            policy.max_open_orders(),
            policy.duplicate_window(),
            policy.session(),
        )
        .with_limits(
            policy.max_risk_percent(),
            policy.max_daily_loss_percent(),
            policy.max_peak_drawdown_percent(),
            policy.max_net_factor_lots(),
        )
    }

    /// Facts that already hold an order on `symbol`.
    fn facts_holding(symbol: &str) -> Option<AccountFacts> {
        let mut facts = facts_with(1, 0.01).expect("facts");
        facts.open_symbols = vec![crate::broker::Symbol::parse(symbol).expect("symbol")];
        Some(facts)
    }

    /// Epoch plus `hour` hours, so session tests are deterministic.
    fn at(hour: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(hour * 3_600)
    }

    fn expect_approved(decision: RiskDecision) -> TradeIntent {
        match decision {
            RiskDecision::Approved(intent) => intent,
            other => panic!("expected approval, got {other:?}"),
        }
    }

    fn expect_rejection(decision: RiskDecision) -> RiskRejection {
        match decision {
            RiskDecision::Rejected(rejection) => rejection,
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn approvals_mint_identity_and_suppress_duplicates_inside_the_window() {
        let gate = RiskGate::new(policy());
        let now = at(10);

        let intent = expect_approved(gate.evaluate(&draft(0.1), facts(0), now));
        assert_eq!(intent.draft().symbol().as_str(), "EURUSD");

        let duplicate =
            expect_rejection(gate.evaluate(&draft(0.1), facts(0), now + Duration::from_secs(30)));
        assert_eq!(duplicate.code(), RiskCode::DuplicateIntent);

        let later =
            expect_approved(gate.evaluate(&draft(0.1), facts(0), now + Duration::from_secs(61)));
        assert_ne!(later.id(), intent.id());
    }

    #[test]
    fn a_zero_window_disables_duplicate_suppression() {
        let gate = RiskGate::new(RiskPolicy::new(
            false,
            vec![symbol()],
            Volume::parse(0.5).expect("volume"),
            Volume::parse(0.5).expect("volume"),
            2,
            Duration::ZERO,
            None,
        ));
        let now = at(10);
        assert!(matches!(
            gate.evaluate(&draft(0.1), facts(0), now),
            RiskDecision::Approved(_)
        ));
        assert!(matches!(
            gate.evaluate(&draft(0.1), facts(0), now),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn kill_switch_wins_over_every_other_control() {
        let gate = RiskGate::new(RiskPolicy::new(
            true,
            Vec::new(),
            Volume::MINIMUM,
            Volume::MINIMUM,
            0,
            Duration::ZERO,
            None,
        ));
        let rejection = expect_rejection(gate.evaluate(&draft(0.01), None, at(3)));
        assert_eq!(rejection.code(), RiskCode::KillSwitch);
        assert_eq!(rejection.detail(), RiskCode::KillSwitch.detail());
    }

    #[test]
    fn allowlist_rejects_unknown_and_when_empty_rejects_all() {
        let gate = RiskGate::new(policy());
        let other = TradeIntentDraft::new(
            parse_instrument("GBPUSD").expect("symbol"),
            Side::Sell,
            OrderKind::Market,
            Volume::parse(0.01).expect("volume"),
            None,
            None,
            None,
        );
        assert_eq!(
            expect_rejection(gate.evaluate(&other, facts(0), at(10))).code(),
            RiskCode::SymbolNotAllowed
        );

        let empty = RiskGate::new(RiskPolicy::default());
        assert_eq!(
            expect_rejection(empty.evaluate(&draft(0.01), facts(0), at(10))).code(),
            RiskCode::SymbolNotAllowed
        );
    }

    #[test]
    fn session_windows_close_and_wrap_in_utc() {
        let gate = RiskGate::new(RiskPolicy::new(
            false,
            vec![symbol()],
            Volume::parse(0.5).expect("volume"),
            Volume::parse(0.5).expect("volume"),
            2,
            Duration::ZERO,
            Some(SessionWindow::parse("7-21").expect("window")),
        ));
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), facts(0), at(3))).code(),
            RiskCode::SessionClosed
        );
        assert!(matches!(
            gate.evaluate(&draft(0.1), facts(0), at(10)),
            RiskDecision::Approved(_)
        ));

        let wrapping = RiskGate::new(RiskPolicy::new(
            false,
            vec![symbol()],
            Volume::parse(0.5).expect("volume"),
            Volume::parse(0.5).expect("volume"),
            2,
            Duration::ZERO,
            Some(SessionWindow::parse("22-6").expect("window")),
        ));
        assert!(matches!(
            wrapping.evaluate(&draft(0.1), facts(0), at(23)),
            RiskDecision::Approved(_)
        ));
        assert_eq!(
            expect_rejection(wrapping.evaluate(&draft(0.1), facts(0), at(12))).code(),
            RiskCode::SessionClosed
        );
    }

    #[test]
    fn volume_and_account_controls_each_reject_with_their_code() {
        let gate = RiskGate::new(policy());
        let now = at(10);
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.6), facts(0), now)).code(),
            RiskCode::VolumeAboveLimit
        );
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), None, now)).code(),
            RiskCode::AccountStateUnavailable
        );
        let closed_account = Some(AccountFacts {
            trade_allowed: false,
            open_orders: 0,
            open_lots: 0.0,
            open_symbols: Vec::new(),
            equity: Some(1_000.0),
            open_positions: Vec::new(),
            prices: Vec::new(),
            day_drawdown_percent: None,
            peak_drawdown_percent: None,
        });
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), closed_account, now)).code(),
            RiskCode::TradingNotAllowed
        );
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), facts(2), now)).code(),
            RiskCode::OrderLimitReached
        );
        assert!(matches!(
            gate.evaluate(&draft(0.1), facts(1), now),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn total_exposure_is_capped() {
        let gate = RiskGate::new(RiskPolicy::new(
            false,
            vec![symbol()],
            Volume::parse(0.5).expect("volume"),
            Volume::parse(0.05).expect("volume"),
            2,
            Duration::ZERO,
            None,
        ));
        let now = at(10);

        // Open volume plus the request may equal the cap, but never exceed it.
        assert!(matches!(
            gate.evaluate(&draft(0.02), facts_with(0, 0.03), now),
            RiskDecision::Approved(_)
        ));
        let rejection = expect_rejection(gate.evaluate(&draft(0.02), facts_with(0, 0.04), now));
        assert_eq!(rejection.code(), RiskCode::ExposureAboveLimit);
        assert_eq!(rejection.detail(), RiskCode::ExposureAboveLimit.detail());
    }

    #[test]
    fn approval_history_is_bounded() {
        let gate = RiskGate::new(RiskPolicy::new(
            false,
            vec![symbol()],
            Volume::parse(1.0).expect("volume"),
            Volume::parse(1.0).expect("volume"),
            2,
            Duration::from_secs(3_600),
            None,
        ));
        let now = at(10);
        for index in 0..(APPROVAL_HISTORY + 20) {
            let price = Price::parse(1.0 + index as f64 / 1_000.0).expect("price");
            let draft = TradeIntentDraft::new(
                symbol(),
                Side::Buy,
                OrderKind::Limit(price),
                Volume::parse(0.01).expect("volume"),
                None,
                None,
                None,
            );
            assert!(matches!(
                gate.evaluate(&draft, facts(0), now),
                RiskDecision::Approved(_)
            ));
        }
        let approvals = gate.approvals.lock().expect("approval lock");
        assert_eq!(approvals.len(), APPROVAL_HISTORY);
    }

    #[test]
    fn poisoned_approval_lock_degrades_to_normal_operation() {
        let gate = RiskGate::new(policy());
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _guard = gate.approvals.lock().expect("approval lock");
            panic!("poison the lock");
        }));
        assert!(poisoned.is_err());

        assert!(matches!(
            gate.evaluate(&draft(0.1), facts(0), at(10)),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn per_trade_risk_rule_rejects_oversized_stops() {
        let gate = rule_policy(5.0, 0.0, 0.0, 0.0);
        let priced = facts_priced(&[("EURUSD", 1.12)]);
        let now = at(10);

        // 0.1 lots, 200-pip stop: $200 on $1,000 = 20% > 5%.
        let big = draft_with_stop("EURUSD", 0.1, 1.10);
        assert_eq!(
            expect_rejection(gate.evaluate(&big, Some(priced.clone()), now)).code(),
            RiskCode::RiskAboveLimit
        );

        // 0.01 lots, 20-pip stop: $2 on $1,000 = 0.2% passes.
        let small = draft_with_stop("EURUSD", 0.01, 1.118);
        assert!(matches!(
            gate.evaluate(&small, Some(priced), now),
            RiskDecision::Approved(_)
        ));

        // A known price on an unpriceable instrument fails closed: a metal
        // quoted in a non-USD currency has no conversion path.
        let mut unpriceable_policy = policy().with_limits(5.0, 0.0, 0.0, 0.0);
        let mut symbols = unpriceable_policy.symbols().to_vec();
        symbols.push(Symbol::parse("XAUEUR").expect("symbol"));
        unpriceable_policy = policy_with_symbols(unpriceable_policy, symbols);
        let unpriceable_gate = RiskGate::new(unpriceable_policy);
        let euro_gold = draft_with_stop("XAUEUR", 0.1, 3_990.0);
        assert_eq!(
            expect_rejection(unpriceable_gate.evaluate(
                &euro_gold,
                Some(facts_priced(&[("XAUEUR", 4_000.0)])),
                now
            ))
            .code(),
            RiskCode::RiskUnverifiable
        );

        // Gold is priceable now: it passes when the risk fits and is rejected
        // by the cap when it does not.
        let mut gold_policy = policy().with_limits(5.0, 0.0, 0.0, 0.0);
        let mut symbols = gold_policy.symbols().to_vec();
        symbols.push(Symbol::parse("XAUUSD").expect("symbol"));
        gold_policy = policy_with_symbols(gold_policy, symbols);
        let gold_gate = RiskGate::new(gold_policy);
        let priced_gold = facts_priced(&[("XAUUSD", 4_341.0)]);
        let tiny = draft_with_stop("XAUUSD", 0.01, 4_340.0);
        assert!(matches!(
            gold_gate.evaluate(&tiny, Some(priced_gold.clone()), now),
            RiskDecision::Approved(_)
        ));
        // A $61 gold stop on 1 oz is $61 = 6.1% of $1,000, above the 5% cap.
        let wide = draft_with_stop("XAUUSD", 0.01, 4_280.0);
        assert_eq!(
            expect_rejection(gold_gate.evaluate(&wide, Some(priced_gold), now)).code(),
            RiskCode::RiskAboveLimit
        );

        // Without any price the rule cannot run and is skipped; the caps stay
        // the binding controls.
        assert!(matches!(
            gate.evaluate(&big, Some(facts_test(0)), now),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn drawdown_breakers_reject_new_risk() {
        let gate = rule_policy(0.0, 10.0, 25.0, 0.0);
        let now = at(10);
        let mut facts = facts_test(0);

        facts.day_drawdown_percent = Some(12.0);
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), Some(facts.clone()), now)).code(),
            RiskCode::DailyLossLimitReached
        );

        facts.day_drawdown_percent = Some(4.0);
        facts.peak_drawdown_percent = Some(30.0);
        assert_eq!(
            expect_rejection(gate.evaluate(&draft(0.1), Some(facts.clone()), now)).code(),
            RiskCode::PeakDrawdownLimitReached
        );

        facts.peak_drawdown_percent = Some(6.0);
        assert!(matches!(
            gate.evaluate(&draft(0.1), Some(facts), now),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn net_factor_cap_blocks_doubling_one_bet() {
        let now = at(10);

        // Short EURUSD is long USD; a long USDJPY would double the bet.
        let facts = facts_positioned("EURUSD", Side::Sell, 0.01);
        let mut both_policy = policy().with_limits(0.0, 0.0, 0.0, 0.01);
        let mut symbols = both_policy.symbols().to_vec();
        symbols.push(Symbol::parse("USDJPY").expect("symbol"));
        both_policy = policy_with_symbols(both_policy, symbols);
        let both_gate = RiskGate::new(both_policy);

        let jpy = draft_with_stop("USDJPY", 0.01, 155.90);
        assert_eq!(
            expect_rejection(both_gate.evaluate(&jpy, Some(facts.clone()), now)).code(),
            RiskCode::FactorExposureAboveLimit
        );

        // Selling the JPY pair cancels the USD exposure and passes.
        let offset = draft_sided("USDJPY", Side::Sell, 0.01, 156.10);
        assert!(matches!(
            both_gate.evaluate(&offset, Some(facts), now),
            RiskDecision::Approved(_)
        ));

        // A disabled factor cap approves the same doubling draft.
        let mut disabled_policy = policy().with_limits(0.0, 0.0, 0.0, 0.0);
        let mut symbols = disabled_policy.symbols().to_vec();
        symbols.push(Symbol::parse("USDJPY").expect("symbol"));
        disabled_policy = policy_with_symbols(disabled_policy, symbols);
        let disabled = RiskGate::new(disabled_policy);
        assert!(matches!(
            disabled.evaluate(
                &jpy,
                Some(facts_positioned("EURUSD", Side::Sell, 0.01)),
                now
            ),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn codes_expose_stable_wire_names_and_details() {
        let codes = [
            RiskCode::KillSwitch,
            RiskCode::SymbolNotAllowed,
            RiskCode::SessionClosed,
            RiskCode::VolumeAboveLimit,
            RiskCode::AccountStateUnavailable,
            RiskCode::TradingNotAllowed,
            RiskCode::OrderLimitReached,
            RiskCode::ExposureAboveLimit,
            RiskCode::DuplicateIntent,
        ];
        for code in codes {
            assert!(!code.as_str().is_empty());
            assert!(!code.detail().is_empty());
            assert_eq!(RiskRejection { code }.detail(), code.detail());
        }
    }
}
