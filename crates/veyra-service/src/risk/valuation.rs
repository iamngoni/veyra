//! Position valuation for deterministic risk checks.
//!
//! Two questions the gate must answer with numbers rather than vibes: how much
//! would this draft lose at its stop (as a percentage of equity), and how much
//! net directional exposure would it add? Both are pure functions of the
//! draft, the caller-supplied reference prices, and the open book.
//!
//! Conventions: one standard lot is 100,000 units; a pip is 0.01 for JPY
//! quotes and 0.0001 otherwise. Pip value is only derived for USD-quoted and
//! USD-based pairs; anything else (crosses, metals, synthetic symbols) is
//! deliberately unpriceable and reported as such.

use crate::broker::Symbol;
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

/// Splits a symbol into its base and quote currencies when it is a plain
/// six-letter pair, ignoring a broker suffix such as `EURUSD.raw`.
pub fn currency_pair(symbol: &Symbol) -> Option<(String, String)> {
    let uppercase = symbol.as_str().to_ascii_uppercase();
    let core = uppercase.split('.').next().unwrap_or("");
    if core.len() != 6 || !core.is_ascii() {
        return None;
    }
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
    let uppercase = symbol.as_str().to_ascii_uppercase();
    let core = uppercase.split('.').next().unwrap_or("");
    if core.len() != 6 || !core.is_ascii() {
        return None;
    }
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

/// Value of one pip for one standard lot, in USD. Only USD-quoted and
/// USD-based instruments are priced; everything else (crosses, non-USD
/// metals) stays deliberately unpriceable.
pub fn pip_value_per_lot(symbol: &Symbol, price: f64) -> Option<f64> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    if let Some((base, quote)) = currency_pair(symbol) {
        let quote_per_pip = pip_size(symbol)? * UNITS_PER_LOT;
        return if quote == "USD" {
            Some(quote_per_pip)
        } else if base == "USD" {
            Some(quote_per_pip / price)
        } else {
            None
        };
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
/// a draft with its own price uses that instead. `None` means the draft cannot
/// be valued at all (no stop, no price, unpriceable symbol, unusable equity).
pub fn risk_percent(
    draft: &TradeIntentDraft,
    reference_price: Option<f64>,
    equity: f64,
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
    let value = pip_value_per_lot(draft.symbol(), entry)?;
    let risk = (distance / pip) * value * draft.volume().value();
    Some(risk / equity * 100.0)
}

/// USD direction of one long lot of `symbol`: +1 longs USD, -1 shorts it,
/// `None` for instruments this model does not classify. USD-quoted metals
/// behave like the currency pairs: long gold is short dollars.
fn usd_sign(symbol: &Symbol) -> Option<f64> {
    if let Some((base, quote)) = currency_pair(symbol) {
        return if quote == "USD" {
            Some(-1.0)
        } else if base == "USD" {
            Some(1.0)
        } else {
            None
        };
    }
    let metal = metal_spec(symbol)?;
    if metal.quote_is_usd { Some(-1.0) } else { None }
}

fn side_sign(side: Side) -> f64 {
    match side {
        Side::Buy => 1.0,
        Side::Sell => -1.0,
    }
}

/// Net USD-directional exposure in lots after adding `draft` to the open book
/// (positive = net long USD). Instruments this model does not classify
/// contribute nothing.
pub fn net_usd_lots(positions: &[PositionFact], draft: &TradeIntentDraft) -> f64 {
    let mut net = 0.0;
    for position in positions {
        if let Some(sign) = usd_sign(&position.symbol) {
            net += sign * side_sign(position.side) * position.lots;
        }
    }
    if let Some(sign) = usd_sign(draft.symbol()) {
        net += sign * side_sign(draft.side()) * draft.volume().value();
    }
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

        // EURUSD: one pip on one lot is ten dollars, whatever the price.
        let eurusd = pip_value_per_lot(&symbol("EURUSD"), 1.10).expect("value");
        assert!((eurusd - 10.0).abs() < 1e-9);

        // USDJPY: one pip on one lot is 1,000 JPY converted to USD.
        let usdjpy = pip_value_per_lot(&symbol("USDJPY"), 156.0).expect("value");
        assert!((usdjpy - 1_000.0 / 156.0).abs() < 1e-9);

        // Crosses stay unpriceable rather than guessed.
        assert_eq!(pip_value_per_lot(&symbol("EURGBP"), 0.86), None);
        assert_eq!(pip_value_per_lot(&symbol("EURUSD"), 0.0), None);

        // Metals use their own contract: 100 oz of gold, 5,000 of silver,
        // quoted in dollars.
        let gold = pip_value_per_lot(&symbol("XAUUSD"), 4_341.0).expect("gold value");
        assert!(
            (gold - 1.0).abs() < 1e-9,
            "one cent on 100 oz is $1, got {gold}"
        );
        let silver = pip_value_per_lot(&symbol("XAGUSD"), 40.0).expect("silver value");
        assert!(
            (silver - 5.0).abs() < 1e-9,
            "0.001 on 5,000 oz is $5, got {silver}"
        );
        assert_eq!(pip_value_per_lot(&symbol("XAUEUR"), 4_000.0), None);
        assert_eq!(pip_value_per_lot(&symbol("XAUOIL"), 1.0), None);
    }

    #[test]
    fn risk_percent_values_the_stop_distance() {
        // EURUSD, 0.01 lots, 20-pip stop: 20 × $0.10 = $2 on $100 equity.
        let small = draft("EURUSD", Side::Buy, 0.01, Some(1.0980), None);
        let risk = risk_percent(&small, Some(1.1000), 100.0).expect("valued");
        assert!((risk - 2.0).abs() < 1e-9, "expected 2%, got {risk}");

        // A draft's own price wins over the caller reference.
        let limit = draft("EURUSD", Side::Buy, 0.01, Some(1.0980), Some(1.1000));
        let risk = risk_percent(&limit, Some(1.1200), 100.0).expect("valued");
        assert!((risk - 2.0).abs() < 1e-9, "expected 2%, got {risk}");

        // USDJPY values in USD through the price.
        let jpy = draft("USDJPY", Side::Buy, 0.01, Some(155.90), None);
        let risk = risk_percent(&jpy, Some(156.00), 100.0).expect("valued");
        let expected = (0.10 / 0.01) * (1_000.0 / 156.0) * 0.01 / 100.0 * 100.0;
        assert!(
            (risk - expected).abs() < 1e-9,
            "expected {expected}%, got {risk}"
        );

        // Gold: a one-dollar stop on one ounce risks one dollar.
        let gold = draft("XAUUSD", Side::Buy, 0.01, Some(4_340.0), None);
        let risk = risk_percent(&gold, Some(4_341.0), 100.0).expect("gold valued");
        assert!((risk - 1.0).abs() < 1e-9, "expected 1%, got {risk}");

        // Unpriceable or incomplete drafts return None.
        assert_eq!(risk_percent(&small, None, 100.0), None);
        assert_eq!(risk_percent(&small, Some(1.10), 0.0), None);
        let no_stop = draft("EURUSD", Side::Buy, 0.01, None, None);
        assert_eq!(risk_percent(&no_stop, Some(1.10), 100.0), None);
        let cross = draft("EURGBP", Side::Buy, 0.01, Some(0.85), None);
        assert_eq!(risk_percent(&cross, Some(0.86), 100.0), None);
    }

    #[test]
    fn net_usd_exposure_signs_each_side() {
        let positions = vec![
            PositionFact {
                symbol: symbol("EURUSD"),
                side: Side::Sell,
                lots: 0.01,
            },
            PositionFact {
                symbol: symbol("USDJPY"),
                side: Side::Buy,
                lots: 0.01,
            },
        ];
        // Short EURUSD is long USD; long USDJPY is long USD.
        let buy_eur = draft("EURUSD", Side::Buy, 0.01, None, None);
        let net = net_usd_lots(&positions, &buy_eur);
        assert!(
            (net - 0.01).abs() < 1e-9,
            "0.01 + 0.01 - 0.01 = 0.01, got {net}"
        );

        // Selling the JPY pair reduces the two USD longs.
        let sell_jpy = draft("USDJPY", Side::Sell, 0.01, None, None);
        let net = net_usd_lots(&positions, &sell_jpy);
        assert!((net - 0.01).abs() < 1e-9, "reduces to 0.01, got {net}");

        // Twice the offset flattens the exposure exactly.
        let flatten = draft("USDJPY", Side::Sell, 0.02, None, None);
        let net = net_usd_lots(&positions, &flatten);
        assert!(net.abs() < 1e-9, "flattens exactly, got {net}");

        // Long gold is short dollars, exactly like a USD-quoted pair.
        let gold = draft("XAUUSD", Side::Buy, 0.01, None, None);
        let net = net_usd_lots(&[], &gold);
        assert!((net + 0.01).abs() < 1e-9, "long gold is -USD, got {net}");

        // An unclassified synthetic contributes nothing.
        let oil = draft("XAUOIL", Side::Buy, 0.01, None, None);
        let net = net_usd_lots(&[], &oil);
        assert_eq!(net, 0.0);
    }
}
