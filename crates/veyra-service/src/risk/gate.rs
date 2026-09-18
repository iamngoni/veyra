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

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use super::RiskPolicy;
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
    policy: RiskPolicy,
    approvals: Arc<Mutex<Vec<Approval>>>,
}

impl RiskGate {
    /// Builds a gate with an empty approval memory.
    pub fn new(policy: RiskPolicy) -> Self {
        Self {
            policy,
            approvals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns the active policy.
    pub fn policy(&self) -> &RiskPolicy {
        &self.policy
    }

    /// Evaluates one draft in the documented check order.
    pub fn evaluate(
        &self,
        draft: &TradeIntentDraft,
        account: Option<AccountFacts>,
        now: SystemTime,
    ) -> RiskDecision {
        if self.policy.kill_switch() {
            return Self::reject(RiskCode::KillSwitch);
        }
        if !self.policy.allows_symbol(draft.symbol()) {
            return Self::reject(RiskCode::SymbolNotAllowed);
        }
        if let Some(window) = self.policy.session()
            && !window.contains(utc_hour(now))
        {
            return Self::reject(RiskCode::SessionClosed);
        }
        if draft.volume().value() > self.policy.max_volume_per_order().value() {
            return Self::reject(RiskCode::VolumeAboveLimit);
        }
        let Some(account) = account else {
            return Self::reject(RiskCode::AccountStateUnavailable);
        };
        if !account.trade_allowed {
            return Self::reject(RiskCode::TradingNotAllowed);
        }
        if account
            .open_symbols
            .iter()
            .any(|open| open == draft.symbol())
        {
            return Self::reject(RiskCode::SymbolAlreadyOpen);
        }
        if account.open_orders >= self.policy.max_open_orders() {
            return Self::reject(RiskCode::OrderLimitReached);
        }
        if account.open_lots + draft.volume().value()
            > self.policy.max_total_lots().value() + EXPOSURE_EPSILON
        {
            return Self::reject(RiskCode::ExposureAboveLimit);
        }
        if self.suppress_duplicate(draft, now) {
            return Self::reject(RiskCode::DuplicateIntent);
        }
        RiskDecision::Approved(TradeIntent::approve(draft.clone()))
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
        let window = self.policy.duplicate_window();
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
        })
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
