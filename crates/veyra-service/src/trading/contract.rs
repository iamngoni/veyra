//! Pre-queue validation against the venue's instrument contract.
//!
//! The risk gate approves a draft on policy, exposure, and stop risk. These
//! checks add the venue facts the gate does not fetch: the instrument's lot
//! band and step, its margin requirement against available free margin, and
//! the spread and minimum stop distance that decide whether a stop can
//! survive execution. Every input is an already validated type, so each check
//! is a pure function and the same rules run in tests and production.

use crate::broker::SymbolSpecPayload;
use crate::trading::intent::TradeIntentDraft;

/// Slack when comparing volumes against the venue's lot grid, in lots.
const VOLUME_EPSILON: f64 = 1e-9;

/// Slack when checking that a volume lands on the lot-step grid.
const STEP_EPSILON: f64 = 1e-6;

/// Stable codes for the pre-queue contract checks; additions are
/// backwards-compatible for consumers that match on the string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContractViolation {
    /// No venue contract was available for the draft's instrument.
    SpecUnavailable,
    /// The draft carries no protective stop to measure.
    MissingStop,
    /// Requested volume is below the venue minimum.
    VolumeBelowMinimum,
    /// Requested volume exceeds the venue maximum.
    VolumeAboveMaximum,
    /// Requested volume is not on the venue's lot-step grid.
    VolumeNotOnStep,
    /// Estimated margin exceeds the account's reported free margin.
    InsufficientMargin,
    /// The stop sits inside the current spread.
    StopInsideSpread,
    /// The stop is closer to the entry than the venue's minimum stop level.
    StopBelowLevel,
    /// The stop sits inside the instrument's recent true range, where normal
    /// noise would close it before the thesis can play out.
    StopInsideNoise,
    /// The entry price needed to measure the stop distance is unknown.
    PriceUnavailable,
}

impl ContractViolation {
    /// Returns the stable wire name.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SpecUnavailable => "spec_unavailable",
            Self::MissingStop => "missing_stops",
            Self::VolumeBelowMinimum => "volume_below_min",
            Self::VolumeAboveMaximum => "volume_above_max",
            Self::VolumeNotOnStep => "volume_not_on_step",
            Self::InsufficientMargin => "insufficient_margin",
            Self::StopInsideSpread => "stop_inside_spread",
            Self::StopBelowLevel => "stop_below_level",
            Self::StopInsideNoise => "stop_inside_noise",
            Self::PriceUnavailable => "price_unavailable",
        }
    }
}

/// Validates one approved draft against the instrument contract and the
/// account's free margin.
///
/// `free_margin` is the venue-reported free margin; `None` means the latest
/// account snapshot payload has not landed yet. A zero `marginRequired` also
/// means the venue supplied no estimate. In either case the estimate is
/// skipped here because the terminal re-validates margin when the order is
/// sent. All other checks run on the contract alone.
///
/// `reference_price` is the latest trusted price for the instrument (the
/// tick's last close); market orders measure their stop distance from it, and
/// limit/stop orders measure from their own trigger price.
///
/// `atr` is the instrument's ATR(14) from the tick's cached candles and
/// `min_stop_atr_fraction` the configured noise floor (zero disables); when
/// the window is too short to measure, the floor is skipped rather than
/// failing entries the tick cannot assess.
pub(crate) fn validate_entry(
    draft: &TradeIntentDraft,
    spec: Option<&SymbolSpecPayload>,
    free_margin: Option<f64>,
    reference_price: Option<f64>,
    atr: Option<f64>,
    min_stop_atr_fraction: f64,
) -> Result<(), ContractViolation> {
    let spec = spec.ok_or(ContractViolation::SpecUnavailable)?;
    let volume = draft.volume().value();

    if volume + VOLUME_EPSILON < spec.lot_min {
        return Err(ContractViolation::VolumeBelowMinimum);
    }
    if volume > spec.lot_max + VOLUME_EPSILON {
        return Err(ContractViolation::VolumeAboveMaximum);
    }
    let steps = (volume - spec.lot_min) / spec.lot_step;
    if (steps - steps.round()).abs() > STEP_EPSILON {
        return Err(ContractViolation::VolumeNotOnStep);
    }
    if let Some(free_margin) = free_margin
        && spec.margin_required > 0.0
        && spec.margin_for(volume) > free_margin + VOLUME_EPSILON
    {
        return Err(ContractViolation::InsufficientMargin);
    }

    let stop = draft
        .stop_loss()
        .map(|stop| stop.value())
        .ok_or(ContractViolation::MissingStop)?;
    let entry = draft
        .order()
        .price()
        .map(|price| price.value())
        .or(reference_price)
        .ok_or(ContractViolation::PriceUnavailable)?;

    let distance = (entry - stop).abs();
    let spread = f64::from(spec.spread_points) * spec.point;
    if distance <= spread {
        return Err(ContractViolation::StopInsideSpread);
    }
    let stop_level = f64::from(spec.stop_level_points) * spec.point;
    if distance < stop_level {
        return Err(ContractViolation::StopBelowLevel);
    }
    if min_stop_atr_fraction > 0.0
        && let Some(atr) = atr
        && atr > 0.0
        && distance < atr * min_stop_atr_fraction
    {
        return Err(ContractViolation::StopInsideNoise);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::intent::{OrderKind, Price, Side, Volume, parse_instrument};

    fn spec() -> SymbolSpecPayload {
        SymbolSpecPayload {
            symbol: "EURUSD".to_owned(),
            digits: 5,
            point: 0.00001,
            bid: 1.1,
            ask: 1.10012,
            spread_points: 12,
            stop_level_points: 5,
            freeze_level_points: 0,
            lot_min: 0.01,
            lot_max: 100.0,
            lot_step: 0.01,
            tick_value: 0.1,
            tick_size: 0.00001,
            margin_required: 3.29,
            swap_long: -0.72,
            swap_short: -0.31,
            swap_type: 0,
            trade_allowed: true,
        }
    }

    /// Contract checks with the ATR floor disabled, so the order-shape rules
    /// stay isolated from volatility rails.
    fn check(
        draft: &TradeIntentDraft,
        spec: Option<&SymbolSpecPayload>,
        free_margin: Option<f64>,
        reference_price: Option<f64>,
    ) -> Result<(), ContractViolation> {
        validate_entry(draft, spec, free_margin, reference_price, None, 0.0)
    }

    fn draft(order: OrderKind, volume: f64, stop: Option<f64>) -> TradeIntentDraft {
        TradeIntentDraft::new(
            parse_instrument("EURUSD").expect("symbol"),
            Side::Buy,
            order,
            Volume::parse(volume).expect("volume"),
            stop.map(|price| Price::parse(price).expect("price")),
            None,
            None,
        )
    }

    #[test]
    fn valid_entries_pass_the_contract() {
        let market = draft(OrderKind::Market, 0.01, Some(1.0900));
        assert_eq!(
            check(&market, Some(&spec()), Some(20.0), Some(1.0950)),
            Ok(())
        );
        assert_eq!(check(&market, Some(&spec()), None, Some(1.0950)), Ok(()));

        let limit = draft(
            OrderKind::Limit(Price::parse(1.1000).expect("price")),
            0.02,
            Some(1.0900),
        );
        assert_eq!(check(&limit, Some(&spec()), Some(20.0), None), Ok(()));
    }

    #[test]
    fn missing_contracts_stops_and_prices_fail_closed() {
        let market = draft(OrderKind::Market, 0.01, Some(1.0900));
        assert_eq!(
            check(&market, None, Some(20.0), Some(1.0950)),
            Err(ContractViolation::SpecUnavailable)
        );
        let stopless = draft(OrderKind::Market, 0.01, None);
        assert_eq!(
            check(&stopless, Some(&spec()), Some(20.0), Some(1.0950)),
            Err(ContractViolation::MissingStop)
        );
        assert_eq!(
            check(&market, Some(&spec()), Some(20.0), None),
            Err(ContractViolation::PriceUnavailable)
        );
    }

    #[test]
    fn volumes_outside_the_lot_band_or_grid_are_rejected() {
        let below = draft(OrderKind::Market, 0.005, Some(1.0900));
        assert_eq!(
            check(&below, Some(&spec()), None, Some(1.0950)),
            Err(ContractViolation::VolumeBelowMinimum)
        );

        let mut narrow = spec();
        narrow.lot_max = 0.02;
        let above = draft(OrderKind::Market, 0.03, Some(1.0900));
        assert_eq!(
            check(&above, Some(&narrow), None, Some(1.0950)),
            Err(ContractViolation::VolumeAboveMaximum)
        );

        let mut coarse = spec();
        coarse.lot_step = 0.02;
        let off_grid = draft(OrderKind::Market, 0.02, Some(1.0900));
        assert_eq!(
            check(&off_grid, Some(&coarse), None, Some(1.0950)),
            Err(ContractViolation::VolumeNotOnStep)
        );
        let on_grid = draft(OrderKind::Market, 0.05, Some(1.0900));
        assert_eq!(check(&on_grid, Some(&coarse), None, Some(1.0950)), Ok(()));
    }

    #[test]
    fn margin_above_free_margin_is_rejected_when_known() {
        let expensive = draft(OrderKind::Market, 0.02, Some(1.0900));
        // 0.02 lots at 3.29 per lot needs 0.0658; 0.05 free margin cannot cover it.
        assert_eq!(
            check(&expensive, Some(&spec()), Some(0.05), Some(1.0950)),
            Err(ContractViolation::InsufficientMargin)
        );
        assert_eq!(
            check(&expensive, Some(&spec()), Some(0.07), Some(1.0950)),
            Ok(())
        );

        let mut estimate_unavailable = spec();
        estimate_unavailable.margin_required = 0.0;
        assert_eq!(
            check(
                &expensive,
                Some(&estimate_unavailable),
                Some(0.0),
                Some(1.0950)
            ),
            Ok(())
        );
    }

    #[test]
    fn stops_inside_the_spread_or_level_are_rejected() {
        // 12 points of spread = 0.00012 in price.
        let tight = draft(OrderKind::Market, 0.01, Some(1.09495));
        assert_eq!(
            check(&tight, Some(&spec()), None, Some(1.0950)),
            Err(ContractViolation::StopInsideSpread)
        );

        let mut level = spec();
        level.spread_points = 0;
        level.stop_level_points = 300;
        let close = draft(OrderKind::Market, 0.01, Some(1.0930));
        assert_eq!(
            check(&close, Some(&level), None, Some(1.0950)),
            Err(ContractViolation::StopBelowLevel)
        );
        let wide = draft(OrderKind::Market, 0.01, Some(1.0900));
        assert_eq!(check(&wide, Some(&level), None, Some(1.0950)), Ok(()));
    }

    #[test]
    fn stops_inside_the_configured_atr_floor_are_rejected() {
        let market = draft(OrderKind::Market, 0.01, Some(1.0943));
        // Distance is 70 points; ATR is 100 points, so a 0.5 floor needs 50
        // points and passes, while a 0.75 floor needs 75 and fails.
        assert_eq!(
            validate_entry(&market, Some(&spec()), None, Some(1.0950), Some(0.001), 0.5),
            Ok(())
        );
        assert_eq!(
            validate_entry(
                &market,
                Some(&spec()),
                None,
                Some(1.0950),
                Some(0.001),
                0.75
            ),
            Err(ContractViolation::StopInsideNoise)
        );
        // An unmeasurable ATR or a disabled floor leaves the shape rules in
        // charge instead of failing the entry.
        assert_eq!(
            validate_entry(&market, Some(&spec()), None, Some(1.0950), None, 0.75),
            Ok(())
        );
        assert_eq!(
            validate_entry(&market, Some(&spec()), None, Some(1.0950), Some(0.001), 0.0),
            Ok(())
        );
    }

    #[test]
    fn violations_carry_stable_codes() {
        let codes = [
            (ContractViolation::SpecUnavailable, "spec_unavailable"),
            (ContractViolation::MissingStop, "missing_stops"),
            (ContractViolation::VolumeBelowMinimum, "volume_below_min"),
            (ContractViolation::VolumeAboveMaximum, "volume_above_max"),
            (ContractViolation::VolumeNotOnStep, "volume_not_on_step"),
            (ContractViolation::InsufficientMargin, "insufficient_margin"),
            (ContractViolation::StopInsideSpread, "stop_inside_spread"),
            (ContractViolation::StopBelowLevel, "stop_below_level"),
            (ContractViolation::StopInsideNoise, "stop_inside_noise"),
            (ContractViolation::PriceUnavailable, "price_unavailable"),
        ];
        for (violation, expected) in codes {
            assert_eq!(violation.as_str(), expected);
        }
    }
}
