//! Position valuation for deterministic risk checks.
//!
//! Two questions the gate must answer with numbers rather than vibes: how much
//! would this draft lose at its stop (as a percentage of equity), and how would
//! it move the net exposure to each currency or asset it trades? Both are pure
//! functions of the draft, the caller-supplied reference prices, and the open
//! book.
//!
//! Conventions: one standard lot is 100,000 units; a pip is 0.01 for JPY
//! quotes and 0.0001 otherwise. A pip is worth `pip_size x units` of the quote
//! currency, so pricing it in USD needs one conversion: nothing for a
//! USD-quoted pair, the pair's own price for a USD-based one, and the quote
//! currency's USD leg for a cross (`EURJPY` prices through `USDJPY`). A cross
//! whose leg the caller does not report is unpriceable rather than
//! approximated, and metals stay USD-quoted by construction.

use crate::broker::{Symbol, SymbolSpecPayload};
use crate::trading::intent::{Side, TradeIntentDraft};

/// Currencies this valuation understands.
const CURRENCIES: [&str; 8] = ["USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "NZD"];

/// One standard lot expressed in units of the base currency.
const UNITS_PER_LOT: f64 = 100_000.0;

/// Physical contract of one metal instrument per standard lot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MetalSpec {
    /// Troy ounces (or equivalent units) per standard lot.
    units_per_lot: f64,
    /// Price increment this venue quotes as one pip.
    pip_size: f64,
    /// Currency the metal is quoted in.
    quote: &'static str,
    /// Whether the quote currency is USD.
    quote_is_usd: bool,
}

/// One open venue position as the risk rules see it.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionFact {
    /// Instrument.
    pub symbol: Symbol,
    /// Direction of the position.
    pub side: Side,
    /// Volume in lots.
    pub lots: f64,
}

/// Splits a symbol into its base and quote currencies when its six-letter
/// code is a pair of known currencies, ignoring a broker's prefix or suffix
/// (`EURUSDm`, `EURUSD.raw`, `#EURUSD`).
pub fn currency_pair(symbol: &Symbol) -> Option<(String, String)> {
    let core = symbol.six_letter_code()?;
    let (base, quote) = core.split_at(3);
    if CURRENCIES.contains(&base) && CURRENCIES.contains(&quote) {
        Some((base.to_owned(), quote.to_owned()))
    } else {
        None
    }
}

/// Splits `XAUUSD`-style metals into their contract spec when the quote is a
/// known currency. Other synthetic symbols return `None`.
fn metal_spec(symbol: &Symbol) -> Option<MetalSpec> {
    let core = symbol.six_letter_code()?;
    let (base, quote) = core.split_at(3);
    if !CURRENCIES.contains(&quote) {
        return None;
    }
    let (units_per_lot, pip_size) = match base {
        "XAU" => (100.0, 0.01),
        "XAG" => (5_000.0, 0.001),
        _ => return None,
    };
    Some(MetalSpec {
        units_per_lot,
        pip_size,
        quote: if quote == "USD" { "USD" } else { "OTHER" },
        quote_is_usd: quote == "USD",
    })
}

/// Pip size for a priced instrument: JPY quotes in hundredths, metals per
/// their contract convention.
pub fn pip_size(symbol: &Symbol) -> Option<f64> {
    if let Some((_, quote)) = currency_pair(symbol) {
        return Some(if quote == "JPY" { 0.01 } else { 0.0001 });
    }
    metal_spec(symbol).map(|metal| metal.pip_size)
}

/// Whether the built-in FX/metal contract model can value `symbol` without a
/// live venue specification.
pub fn supports_static_valuation(symbol: &Symbol) -> bool {
    pip_size(symbol).is_some()
}

/// USD per one unit of `quote`, as this valuation sees it.
///
/// USD-quoted instruments need no conversion. A USD-based pair carries its own
/// conversion in `price` (`USDJPY` at 156 is 156 yen per dollar, so one yen is
/// 1/156). Anything else is a cross, and its quote currency is converted
/// through the USD leg the caller reports — `USDXXX` inverted, `XXXUSD` as
/// written. A missing or unusable leg returns `None`, which the callers treat
/// as unpriceable rather than approximated.
fn quote_in_usd(base: &str, quote: &str, price: f64, prices: &[(Symbol, f64)]) -> Option<f64> {
    if quote == "USD" {
        return Some(1.0);
    }
    if base == "USD" {
        return Some(1.0 / price);
    }
    let direct = format!("USD{quote}");
    if let Some((_, leg)) = prices
        .iter()
        .find(|(symbol, _)| symbol.as_str().eq_ignore_ascii_case(&direct))
        && leg.is_finite()
        && *leg > 0.0
    {
        return Some(1.0 / *leg);
    }
    let inverse = format!("{quote}USD");
    prices
        .iter()
        .find(|(symbol, _)| symbol.as_str().eq_ignore_ascii_case(&inverse))
        .map(|(_, leg)| *leg)
        .filter(|rate| rate.is_finite() && *rate > 0.0)
}

/// Value of one pip for one standard lot, in USD.
///
/// `prices` are the caller's other reference prices. They are only read to
/// convert a cross's quote currency; USD-quoted and USD-based instruments
/// price from `price` alone, and passing an empty slice prices every
/// instrument with a USD leg.
pub fn pip_value_per_lot(symbol: &Symbol, price: f64, prices: &[(Symbol, f64)]) -> Option<f64> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    if let Some((base, quote)) = currency_pair(symbol) {
        let quote_per_pip = pip_size(symbol)? * UNITS_PER_LOT;
        return Some(quote_per_pip * quote_in_usd(&base, &quote, price, prices)?);
    }
    let metal = metal_spec(symbol)?;
    if metal.quote_is_usd {
        Some(metal.pip_size * metal.units_per_lot)
    } else {
        None
    }
}

/// Percentage of `equity` a draft risks between its entry and its stop.
///
/// `reference_price` is the caller's best entry reference for a market order;
/// a draft with its own price uses that instead. `prices` are the caller's
/// other reference prices, read only to convert a cross's quote currency.
/// `None` means the draft cannot be valued at all (no stop, no price,
/// unpriceable symbol, unusable equity).
pub fn risk_percent(
    draft: &TradeIntentDraft,
    reference_price: Option<f64>,
    equity: f64,
    prices: &[(Symbol, f64)],
) -> Option<f64> {
    if !equity.is_finite() || equity <= 0.0 {
        return None;
    }
    let stop = draft.stop_loss()?.value();
    let entry = draft
        .order()
        .price()
        .map(|price| price.value())
        .or(reference_price)?;
    let distance = (entry - stop).abs();
    if !distance.is_finite() || distance <= 0.0 {
        return None;
    }
    let pip = pip_size(draft.symbol())?;
    let value = pip_value_per_lot(draft.symbol(), entry, prices)?;
    let risk = (distance / pip) * value * draft.volume().value();
    Some(risk / equity * 100.0)
}

/// Percentage of equity at risk using the venue's own tick economics when a
/// live specification is present.
///
/// CFDs, crypto, indices, and broker-specific contracts cannot be valued from
/// their names. For them the loss in account currency is the number of ticks
/// between entry and stop, multiplied by the venue-reported tick value and lot
/// volume. A present but unusable specification never falls back to an
/// approximation; it returns `None` so the gate fails closed.
pub fn risk_percent_with_spec(
    draft: &TradeIntentDraft,
    reference_price: Option<f64>,
    equity: f64,
    prices: &[(Symbol, f64)],
    spec: Option<&SymbolSpecPayload>,
) -> Option<f64> {
    let Some(spec) = spec else {
        return risk_percent(draft, reference_price, equity, prices);
    };
    if spec.symbol != draft.symbol().as_str()
        || !equity.is_finite()
        || equity <= 0.0
        || !spec.tick_size.is_finite()
        || spec.tick_size <= 0.0
        || !spec.tick_value.is_finite()
        || spec.tick_value <= 0.0
    {
        return None;
    }
    let stop = draft.stop_loss()?.value();
    let entry = draft
        .order()
        .price()
        .map(|price| price.value())
        .or(reference_price)?;
    let distance = (entry - stop).abs();
    if !distance.is_finite() || distance <= 0.0 {
        return None;
    }
    let loss = (distance / spec.tick_size) * spec.tick_value * draft.volume().value();
    if !loss.is_finite() || loss <= 0.0 {
        return None;
    }
    Some(loss / equity * 100.0)
}

/// What an open position loses, in account currency, if its stop is hit from
/// `current`, priced with the venue's tick economics. A stop already past
/// `current` on the profitable side has nothing left to lose and counts as
/// zero. `None` without a stop or with unusable contract numbers.
pub fn stop_loss_money(
    spec: &SymbolSpecPayload,
    side: Side,
    lots: f64,
    current: f64,
    stop: f64,
) -> Option<f64> {
    let usable = spec.tick_size.is_finite()
        && spec.tick_size > 0.0
        && spec.tick_value.is_finite()
        && spec.tick_value > 0.0
        && lots.is_finite()
        && lots > 0.0
        && current.is_finite()
        && current > 0.0
        && stop.is_finite()
        && stop > 0.0;
    if !usable {
        return None;
    }
    let adverse = match side {
        Side::Buy => current - stop,
        Side::Sell => stop - current,
    };
    Some(adverse.max(0.0) / spec.tick_size * spec.tick_value * lots)
}

/// Codes the second half of a six-letter instrument may be for the instrument
/// to read as two exposures: fiat currencies (the majors and the exotics
/// brokers list), the metals, and oil. The first half may be any three
/// letters, so `AUDCAD`, `USDTRY`, `XAUEUR`, and `BTCUSD` all split.
const EXPOSURE_QUOTES: &[&str] = &[
    "USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "NZD", "CNH", "CZK", "DKK", "HKD", "HUF",
    "MXN", "NOK", "PLN", "RUB", "SEK", "SGD", "TRY", "ZAR", "XAU", "XAG", "OIL",
];

/// The assets one long lot of `symbol` is long (`+1`) and short (`-1`).
///
/// A six-letter code with a known second half is long its first half and
/// short its second, so a short AUDCAD is short AUD and long CAD, and a long
/// XAUUSD is long gold and short dollars. Anything else (`SP500m`, a share)
/// is one exposure to itself.
fn exposure_legs(symbol: &Symbol) -> Vec<(String, f64)> {
    if let Some(code) = symbol.six_letter_code() {
        let (base, quote) = code.split_at(3);
        if EXPOSURE_QUOTES.contains(&quote) {
            return vec![(base.to_owned(), 1.0), (quote.to_owned(), -1.0)];
        }
    }
    vec![(symbol.as_str().to_ascii_uppercase(), 1.0)]
}

fn side_sign(side: Side) -> f64 {
    match side {
        Side::Buy => 1.0,
        Side::Sell => -1.0,
    }
}

/// How a draft would move the net exposure to one asset it trades.
#[derive(Debug, Clone, PartialEq)]
pub struct FactorMove {
    /// Asset code in upper case (`AUD`, `XAU`, `BTC`, `SP500M`).
    pub asset: String,
    /// Net lots on the open book before the draft; positive is net long.
    pub before: f64,
    /// Net lots once the draft is added.
    pub after: f64,
}

/// Signed lots one position or draft holds in `asset` through its legs.
fn held_in(asset: &str, symbol: &Symbol, side: Side, lots: f64) -> f64 {
    exposure_legs(symbol)
        .into_iter()
        .filter(|(leg, _)| leg == asset)
        .map(|(_, sign)| sign * side_sign(side) * lots)
        .sum()
}

/// The net exposure to each asset that `lots` of `symbol` on `side` trades,
/// before and after it is added to the book.
///
/// Every open position counts through all of its legs, so four AUD crosses
/// are one AUD bet however they are quoted, and long EURUSD plus long USDJPY
/// cancel on the dollar while staying long EUR and short JPY.
pub fn factor_moves(
    positions: &[PositionFact],
    symbol: &Symbol,
    side: Side,
    lots: f64,
) -> Vec<FactorMove> {
    exposure_legs(symbol)
        .into_iter()
        .map(|(asset, _)| {
            let before = positions
                .iter()
                .map(|position| held_in(&asset, &position.symbol, position.side, position.lots))
                .sum::<f64>();
            FactorMove {
                after: before + held_in(&asset, symbol, side, lots),
                before,
                asset,
            }
        })
        .collect()
}

/// Slack allowed when comparing summed lot volumes against the cap.
const FACTOR_EPSILON: f64 = 1e-9;

/// Whether `lots` of `symbol` on `side` would take the net exposure to any
/// asset it trades past `cap` lots. Only an exposure the trade grows counts,
/// so a trade that unwinds part of a stacked bet never breaches, even while
/// the book is over the cap. A cap of zero or less is disabled.
pub fn breaches_factor_cap(
    positions: &[PositionFact],
    symbol: &Symbol,
    side: Side,
    lots: f64,
    cap: f64,
) -> bool {
    cap > 0.0
        && factor_moves(positions, symbol, side, lots)
            .iter()
            .any(|motion| {
                motion.after.abs() > cap + FACTOR_EPSILON
                    && motion.after.abs() > motion.before.abs() + FACTOR_EPSILON
            })
}

/// Net lots the book holds in every asset it is exposed to, by asset code,
/// leaving out assets whose legs cancel exactly.
pub fn book_exposure(positions: &[PositionFact]) -> std::collections::BTreeMap<String, f64> {
    let mut net = std::collections::BTreeMap::<String, f64>::new();
    for position in positions {
        for (asset, sign) in exposure_legs(&position.symbol) {
            *net.entry(asset).or_default() += sign * side_sign(position.side) * position.lots;
        }
    }
    net.retain(|_, lots| lots.abs() > FACTOR_EPSILON);
    net
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::intent::{OrderKind, Price, Volume};

    fn symbol(name: &str) -> Symbol {
        Symbol::parse(name).expect("symbol")
    }

    fn draft(
        name: &str,
        side: Side,
        lots: f64,
        stop: Option<f64>,
        price: Option<f64>,
    ) -> TradeIntentDraft {
        let order = match price {
            Some(price) => OrderKind::Limit(Price::parse(price).expect("price")),
            None => OrderKind::Market,
        };
        TradeIntentDraft::new(
            symbol(name),
            side,
            order,
            Volume::parse(lots).expect("volume"),
            stop.map(|stop| Price::parse(stop).expect("stop")),
            None,
            None,
        )
    }

    fn venue_spec(name: &str, tick_size: f64, tick_value: f64) -> SymbolSpecPayload {
        SymbolSpecPayload {
            currency_base: None,
            currency_profit: None,
            sessions: Vec::new(),
            symbol: name.to_owned(),
            digits: 2,
            point: tick_size,
            bid: 100_000.0,
            ask: 100_001.0,
            spread_points: 1,
            stop_level_points: 0,
            freeze_level_points: 0,
            lot_min: 0.01,
            lot_max: 10.0,
            lot_step: 0.01,
            tick_value,
            tick_size,
            margin_required: 100.0,
            swap_long: 0.0,
            swap_short: 0.0,
            swap_type: 0,
            trade_allowed: true,
        }
    }

    #[test]
    fn pairs_and_pip_values_follow_the_usd_conventions() {
        assert_eq!(
            currency_pair(&symbol("eurusd")),
            Some(("EUR".to_owned(), "USD".to_owned()))
        );
        assert_eq!(
            currency_pair(&symbol("EURUSD.raw")),
            Some(("EUR".to_owned(), "USD".to_owned()))
        );
        assert_eq!(currency_pair(&symbol("XAUUSD")), None);
        assert_eq!(currency_pair(&symbol("EURUSDX")), None);
        // Account-type suffixes keep the pair.
        assert_eq!(
            currency_pair(&symbol("EURUSDm")),
            Some(("EUR".to_owned(), "USD".to_owned()))
        );
        assert_eq!(pip_size(&symbol("USDJPYm")), Some(0.01));

        // EURUSD: one pip on one lot is ten dollars, whatever the price.
        let eurusd = pip_value_per_lot(&symbol("EURUSD"), 1.10, &[]).expect("value");
        assert!((eurusd - 10.0).abs() < 1e-9);

        // USDJPY: one pip on one lot is 1,000 JPY converted to USD.
        let usdjpy = pip_value_per_lot(&symbol("USDJPY"), 156.0, &[]).expect("value");
        assert!((usdjpy - 1_000.0 / 156.0).abs() < 1e-9);

        // Crosses stay unpriceable rather than guessed.
        assert_eq!(
            pip_value_per_lot(&symbol("EURGBP"), 0.86, &[]),
            None,
            "a cross without its USD leg is unpriceable"
        );
        assert_eq!(pip_value_per_lot(&symbol("EURUSD"), 0.0, &[]), None);

        // Metals use their own contract: 100 oz of gold, 5,000 of silver,
        // quoted in dollars.
        let gold = pip_value_per_lot(&symbol("XAUUSD"), 4_341.0, &[]).expect("gold value");
        assert!(
            (gold - 1.0).abs() < 1e-9,
            "one cent on 100 oz is $1, got {gold}"
        );
        let silver = pip_value_per_lot(&symbol("XAGUSD"), 40.0, &[]).expect("silver value");
        assert!(
            (silver - 5.0).abs() < 1e-9,
            "0.001 on 5,000 oz is $5, got {silver}"
        );
        assert_eq!(pip_value_per_lot(&symbol("XAUEUR"), 4_000.0, &[]), None);
        assert_eq!(pip_value_per_lot(&symbol("XAUOIL"), 1.0, &[]), None);
    }

    #[test]
    fn crosses_price_through_their_usd_leg() {
        let legs = |pairs: &[(&str, f64)]| -> Vec<(Symbol, f64)> {
            pairs
                .iter()
                .map(|(name, price)| (symbol(name), *price))
                .collect()
        };

        // EURJPY through USDJPY at 156: one pip on one lot is 1,000 JPY,
        // which is 1,000/156 dollars.
        let usdjpy = legs(&[("USDJPY", 156.0), ("EURUSD", 1.1000)]);
        let jpy_cross = pip_value_per_lot(&symbol("EURJPY"), 156.0, &usdjpy).expect("valued");
        assert!(
            (jpy_cross - 1_000.0 / 156.0).abs() < 1e-9,
            "expected {}, got {jpy_cross}",
            1_000.0 / 156.0
        );

        // The inverse leg states the same rate the other way up.
        let jpyusd = legs(&[("JPYUSD", 1.0 / 156.0)]);
        let same = pip_value_per_lot(&symbol("EURJPY"), 156.0, &jpyusd).expect("valued");
        assert!((same - jpy_cross).abs() < 1e-9);

        // A GBP-quoted cross reads GBPUSD as written.
        let gbp = legs(&[("GBPUSD", 1.27)]);
        let eurgbp = pip_value_per_lot(&symbol("EURGBP"), 0.86, &gbp).expect("valued");
        assert!((eurgbp - 0.0001 * 100_000.0 * 1.27).abs() < 1e-9);

        // An AUD-quoted cross prices the same way, and the legs of an
        // already-converted symbol are ignored.
        let aud = legs(&[("AUDUSD", 0.66)]);
        let audjpy = pip_value_per_lot(&symbol("AUDJPY"), 103.0, &aud);
        assert_eq!(audjpy, None, "AUDJPY needs the JPY leg, not the AUD one");
        let both = legs(&[("AUDUSD", 0.66), ("USDJPY", 156.0)]);
        let valued = pip_value_per_lot(&symbol("AUDJPY"), 103.0, &both).expect("valued");
        assert!((valued - 1_000.0 / 156.0).abs() < 1e-9);

        // Unusable legs are refused rather than approximated.
        for broken in [
            legs(&[]),
            legs(&[("USDJPY", 0.0)]),
            legs(&[("USDJPY", -156.0)]),
            legs(&[("USDJPY", f64::NAN)]),
        ] {
            assert_eq!(pip_value_per_lot(&symbol("EURJPY"), 156.0, &broken), None);
        }
    }

    #[test]
    fn risk_percent_values_the_stop_distance() {
        // EURUSD, 0.01 lots, 20-pip stop: 20 × $0.10 = $2 on $100 equity.
        let small = draft("EURUSD", Side::Buy, 0.01, Some(1.0980), None);
        let risk = risk_percent(&small, Some(1.1000), 100.0, &[]).expect("valued");
        assert!((risk - 2.0).abs() < 1e-9, "expected 2%, got {risk}");

        // A draft's own price wins over the caller reference.
        let limit = draft("EURUSD", Side::Buy, 0.01, Some(1.0980), Some(1.1000));
        let risk = risk_percent(&limit, Some(1.1200), 100.0, &[]).expect("valued");
        assert!((risk - 2.0).abs() < 1e-9, "expected 2%, got {risk}");

        // USDJPY values in USD through the price.
        let jpy = draft("USDJPY", Side::Buy, 0.01, Some(155.90), None);
        let risk = risk_percent(&jpy, Some(156.00), 100.0, &[]).expect("valued");
        let expected = (0.10 / 0.01) * (1_000.0 / 156.0) * 0.01 / 100.0 * 100.0;
        assert!(
            (risk - expected).abs() < 1e-9,
            "expected {expected}%, got {risk}"
        );

        // Gold: a one-dollar stop on one ounce risks one dollar.
        let gold = draft("XAUUSD", Side::Buy, 0.01, Some(4_340.0), None);
        let risk = risk_percent(&gold, Some(4_341.0), 100.0, &[]).expect("gold valued");
        assert!((risk - 1.0).abs() < 1e-9, "expected 1%, got {risk}");

        // Unpriceable or incomplete drafts return None.
        assert_eq!(risk_percent(&small, None, 100.0, &[]), None);
        assert_eq!(risk_percent(&small, Some(1.10), 0.0, &[]), None);
        let no_stop = draft("EURUSD", Side::Buy, 0.01, None, None);
        assert_eq!(risk_percent(&no_stop, Some(1.10), 100.0, &[]), None);
        let cross = draft("EURGBP", Side::Buy, 0.01, Some(0.85), None);
        assert_eq!(
            risk_percent(&cross, Some(0.86), 100.0, &[]),
            None,
            "without the GBP leg the risk stays unverifiable"
        );
        let legs = [("GBPUSD", 1.2700)];
        let priced: Vec<(Symbol, f64)> = legs
            .iter()
            .map(|(name, price)| (symbol(name), *price))
            .collect();
        let risk = risk_percent(&cross, Some(0.86), 100.0, &priced).expect("cross valued");
        // GBPUSD at 1.27: one pip on one lot is 0.0001 x 100,000 x 1.27.
        let expected = (0.01 / 0.0001) * (0.0001 * 100_000.0 * 1.27) * 0.01 / 100.0 * 100.0;
        assert!(
            (risk - expected).abs() < 1e-9,
            "expected {expected}%, got {risk}"
        );
    }

    #[test]
    fn venue_ticks_value_crypto_and_indices_without_name_assumptions() {
        let bitcoin = draft("BTCUSD", Side::Buy, 0.01, Some(99_000.0), None);
        let btc_spec = venue_spec("BTCUSD", 0.01, 0.01);
        let risk = risk_percent_with_spec(&bitcoin, Some(100_000.0), 1_000.0, &[], Some(&btc_spec))
            .expect("bitcoin risk valued");
        assert!((risk - 1.0).abs() < 1e-9, "expected 1%, got {risk}");

        let index = draft("SP500m", Side::Buy, 0.01, Some(5_990.0), None);
        let index_spec = venue_spec("SP500m", 0.1, 1.0);
        let risk = risk_percent_with_spec(&index, Some(6_000.0), 1_000.0, &[], Some(&index_spec))
            .expect("index risk valued");
        assert!((risk - 0.1).abs() < 1e-9, "expected 0.1%, got {risk}");

        let unusable = venue_spec("BTCUSD", 0.01, 0.0);
        assert_eq!(
            risk_percent_with_spec(&bitcoin, Some(100_000.0), 1_000.0, &[], Some(&unusable)),
            None,
            "zero tick value fails closed"
        );
        assert!(!supports_static_valuation(&symbol("BTCUSD")));
        assert!(supports_static_valuation(&symbol("EURUSD")));
    }

    #[test]
    fn an_open_stop_is_valued_from_the_current_price() {
        let spec = venue_spec("NZDCHF", 0.00001, 1.0);
        // 0.01 lot long at 0.47007 now 0.46900, stop 0.46730: 17 pips left.
        let left = stop_loss_money(&spec, Side::Buy, 0.01, 0.469, 0.4673).expect("valued");
        assert!((left - 1.7).abs() < 1e-9, "{left}");
        let short = stop_loss_money(&spec, Side::Sell, 0.01, 0.469, 0.4707).expect("valued");
        assert!((short - 1.7).abs() < 1e-9, "{short}");
        // A stop locked past the current price has nothing left to lose.
        assert_eq!(
            stop_loss_money(&spec, Side::Buy, 0.01, 0.469, 0.4695),
            Some(0.0)
        );
        assert_eq!(
            stop_loss_money(&spec, Side::Buy, 0.01, 0.469, 0.0),
            None,
            "no stop"
        );
        let broken = SymbolSpecPayload {
            tick_size: 0.0,
            ..spec
        };
        assert_eq!(
            stop_loss_money(&broken, Side::Buy, 0.01, 0.469, 0.4673),
            None
        );
    }

    fn position(name: &str, side: Side, lots: f64) -> PositionFact {
        PositionFact {
            symbol: symbol(name),
            side,
            lots,
        }
    }

    fn moves(positions: &[PositionFact], draft: &TradeIntentDraft) -> Vec<(String, f64, f64)> {
        factor_moves(
            positions,
            draft.symbol(),
            draft.side(),
            draft.volume().value(),
        )
        .into_iter()
        .map(|motion| {
            let round = |lots: f64| (lots * 1e6).round() / 1e6;
            (motion.asset, round(motion.before), round(motion.after))
        })
        .collect()
    }

    #[test]
    fn each_side_of_a_pair_is_its_own_exposure() {
        // Short EURUSD is short EUR and long USD; long USDJPY is long USD and
        // short JPY.
        let book = [
            position("EURUSD", Side::Sell, 0.01),
            position("USDJPY", Side::Buy, 0.01),
        ];
        assert_eq!(
            moves(&book, &draft("EURUSD", Side::Buy, 0.01, None, None)),
            [("EUR".into(), -0.01, 0.0), ("USD".into(), 0.02, 0.01)]
        );
        assert_eq!(
            moves(&book, &draft("USDJPY", Side::Sell, 0.02, None, None)),
            [("USD".into(), 0.02, 0.0), ("JPY".into(), -0.01, 0.01)]
        );
    }

    #[test]
    fn crosses_stack_on_the_currency_they_share() {
        // Today's book: four ways of being short the Australian dollar.
        let book = [
            position("AUDCHF", Side::Sell, 0.01),
            position("AUDJPY", Side::Sell, 0.01),
            position("GBPAUD", Side::Buy, 0.01),
        ];
        assert_eq!(
            moves(&book, &draft("AUDCAD", Side::Sell, 0.01, None, None)),
            [("AUD".into(), -0.03, -0.04), ("CAD".into(), 0.0, 0.01)]
        );
        // A long AUDNZD unwinds part of the AUD short.
        assert_eq!(
            moves(&book, &draft("AUDNZD", Side::Buy, 0.01, None, None)),
            [("AUD".into(), -0.03, -0.02), ("NZD".into(), 0.0, -0.01)]
        );
    }

    #[test]
    fn exotics_metals_and_coins_split_into_both_assets() {
        let book = [position("XAUUSD", Side::Buy, 0.01)];
        assert_eq!(
            moves(&book, &draft("USDTRY", Side::Sell, 0.01, None, None)),
            [("USD".into(), -0.01, -0.02), ("TRY".into(), 0.0, 0.01)]
        );
        assert_eq!(
            moves(&book, &draft("BTCUSD", Side::Buy, 0.01, None, None)),
            [("BTC".into(), 0.0, 0.01), ("USD".into(), -0.01, -0.02)]
        );
        assert_eq!(
            moves(&book, &draft("XAUEUR", Side::Buy, 0.01, None, None)),
            [("XAU".into(), 0.01, 0.02), ("EUR".into(), 0.0, -0.01)]
        );
        assert_eq!(
            moves(&[], &draft("XAUOIL", Side::Sell, 0.01, None, None)),
            [("XAU".into(), 0.0, -0.01), ("OIL".into(), 0.0, 0.01)]
        );
        // Broker suffixes do not hide the assets.
        assert_eq!(
            moves(
                &[position("AUDUSDm", Side::Sell, 0.01)],
                &draft("AUDCAD.raw", Side::Sell, 0.01, None, None)
            ),
            [("AUD".into(), -0.01, -0.02), ("CAD".into(), 0.0, 0.01)]
        );
    }

    #[test]
    fn an_instrument_without_two_assets_is_its_own_exposure() {
        let book = [
            position("SP500m", Side::Buy, 0.01),
            position("EURUSD", Side::Buy, 0.01),
        ];
        assert_eq!(
            moves(&book, &draft("sp500m", Side::Buy, 0.01, None, None)),
            [("SP500M".into(), 0.01, 0.02)]
        );
        assert_eq!(
            moves(&book, &draft("Nd100m", Side::Sell, 0.02, None, None)),
            [("ND100M".into(), 0.0, -0.02)]
        );
    }

    #[test]
    fn the_cap_refuses_only_exposure_a_trade_grows() {
        let book = [
            position("AUDCHF", Side::Sell, 0.01),
            position("GBPAUD", Side::Buy, 0.01),
        ];
        let audcad = symbol("AUDCAD");
        assert!(breaches_factor_cap(&book, &audcad, Side::Sell, 0.01, 0.02));
        assert!(!breaches_factor_cap(&book, &audcad, Side::Buy, 0.01, 0.02));
        assert!(!breaches_factor_cap(&book, &audcad, Side::Sell, 0.01, 0.03));
        assert!(
            !breaches_factor_cap(&book, &audcad, Side::Sell, 0.01, 0.0),
            "a zero cap is off"
        );
        // Over the cap already: shrinking passes, growing does not.
        let over = [
            position("AUDCHF", Side::Sell, 0.02),
            position("AUDJPY", Side::Sell, 0.02),
        ];
        assert!(!breaches_factor_cap(
            &over,
            &symbol("AUDNZD"),
            Side::Buy,
            0.01,
            0.02
        ));
        assert!(breaches_factor_cap(
            &over,
            &symbol("NZDCHF"),
            Side::Sell,
            0.01,
            0.02
        ));
    }

    #[test]
    fn the_book_reports_its_net_lots_per_asset() {
        let book = [
            position("AUDCAD", Side::Sell, 0.01),
            position("AUDCHF", Side::Sell, 0.01),
            position("CHFJPY", Side::Buy, 0.01),
            position("USDCAD", Side::Buy, 0.01),
        ];
        let net = book_exposure(&book);
        let rounded: Vec<(&str, f64)> = net
            .iter()
            .map(|(asset, lots)| (asset.as_str(), (lots * 1e6).round() / 1e6))
            .collect();
        // CAD: +0.01 from the AUDCAD short, -0.01 from the USDCAD long.
        assert_eq!(
            rounded,
            [("AUD", -0.02), ("CHF", 0.02), ("JPY", -0.01), ("USD", 0.01)]
        );
        assert!(book_exposure(&[]).is_empty());
    }
}
