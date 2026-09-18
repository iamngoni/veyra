//! Realized-trade performance from the venue's account history.
//!
//! Success rate has to come from closed fills: a floating snapshot taken by
//! the reconciler misses the exit (`position_closed` records the last-seen
//! profit, not the final one). This module is pure — it turns validated
//! [`ClosedTradePayload`] history into a bounded report the control surface
//! and console render — and the route supplies the fills by asking the
//! terminal for its account history.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::broker::ClosedTradePayload;

/// One symbol's contribution to the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolPerformance {
    /// Instrument.
    pub symbol: String,
    /// Closed trades on the instrument.
    pub trades: u32,
    /// Closed trades with a positive net result.
    pub wins: u32,
    /// Net realized profit on the instrument, in account currency.
    pub net_profit: f64,
}

/// Aggregated realized performance for one window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerformanceReport {
    /// Closed trades in the window.
    pub trades: u32,
    /// Trades with net profit above zero.
    pub wins: u32,
    /// Trades with net profit below zero.
    pub losses: u32,
    /// Trades that closed exactly flat.
    pub breakeven: u32,
    /// Wins as a percentage of trades, one decimal.
    pub win_rate_percent: f64,
    /// Net realized profit across every closed trade.
    pub net_profit: f64,
    /// Sum of the winners' net profit.
    pub gross_profit: f64,
    /// The losers' net loss as a positive value.
    pub gross_loss: f64,
    /// `gross_profit / gross_loss`; `None` while nothing has lost.
    pub profit_factor: Option<f64>,
    /// Mean net profit of the winners; `None` without winners.
    pub average_win: Option<f64>,
    /// Mean net loss of the losers as a positive value; `None` without losers.
    pub average_loss: Option<f64>,
    /// Net profit per trade; `None` without trades.
    pub expectancy: Option<f64>,
    /// Best single-trade net profit; `None` without trades.
    pub best_trade: Option<f64>,
    /// Worst single-trade net profit; `None` without trades.
    pub worst_trade: Option<f64>,
    /// Per-symbol totals, most-traded first.
    pub by_symbol: Vec<SymbolPerformance>,
}

/// Aggregates closed trades into a report. Every derived number is rounded to
/// two decimals (the win rate to one) so the JSON stays stable and readable.
pub fn summarize(trades: &[ClosedTradePayload]) -> PerformanceReport {
    let mut wins = 0_u32;
    let mut losses = 0_u32;
    let mut breakeven = 0_u32;
    let mut gross_profit = 0.0_f64;
    let mut gross_loss = 0.0_f64;
    let mut net_profit = 0.0_f64;
    let mut best: Option<f64> = None;
    let mut worst: Option<f64> = None;
    let mut by_symbol: BTreeMap<&str, SymbolPerformance> = BTreeMap::new();

    for trade in trades {
        let profit = trade.net_profit();
        net_profit += profit;
        match profit.partial_cmp(&0.0) {
            Some(std::cmp::Ordering::Greater) => {
                wins += 1;
                gross_profit += profit;
            }
            Some(std::cmp::Ordering::Less) => {
                losses += 1;
                gross_loss += -profit;
            }
            _ => breakeven += 1,
        }
        best = Some(best.map_or(profit, |current| current.max(profit)));
        worst = Some(worst.map_or(profit, |current| current.min(profit)));
        let entry = by_symbol
            .entry(trade.symbol.as_str())
            .or_insert_with(|| SymbolPerformance {
                symbol: trade.symbol.clone(),
                trades: 0,
                wins: 0,
                net_profit: 0.0,
            });
        entry.trades += 1;
        if profit > 0.0 {
            entry.wins += 1;
        }
        entry.net_profit += profit;
    }

    let trades_count = trades.len() as u32;
    let win_rate_percent = if trades_count == 0 {
        0.0
    } else {
        round_to(f64::from(wins) / f64::from(trades_count) * 100.0, 1)
    };
    let profit_factor = (gross_loss > 0.0).then(|| round_to(gross_profit / gross_loss, 2));
    let average_win = (wins > 0).then(|| round_to(gross_profit / f64::from(wins), 2));
    let average_loss = (losses > 0).then(|| round_to(gross_loss / f64::from(losses), 2));
    let expectancy = (trades_count > 0).then(|| round_to(net_profit / f64::from(trades_count), 2));

    let mut by_symbol: Vec<SymbolPerformance> = by_symbol
        .into_values()
        .map(|mut entry| {
            entry.net_profit = round_to(entry.net_profit, 2);
            entry
        })
        .collect();
    by_symbol.sort_by(|left, right| {
        right
            .trades
            .cmp(&left.trades)
            .then_with(|| left.symbol.cmp(&right.symbol))
    });

    PerformanceReport {
        trades: trades_count,
        wins,
        losses,
        breakeven,
        win_rate_percent,
        net_profit: round_to(net_profit, 2),
        gross_profit: round_to(gross_profit, 2),
        gross_loss: round_to(gross_loss, 2),
        profit_factor,
        average_win,
        average_loss,
        expectancy,
        best_trade: best.map(|value| round_to(value, 2)),
        worst_trade: worst.map(|value| round_to(value, 2)),
        by_symbol,
    }
}

/// Rounds to `decimals` places.
fn round_to(value: f64, decimals: i32) -> f64 {
    let factor = 10_f64.powi(decimals);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::PositionKind;

    fn trade(symbol: &str, profit: f64, swap: f64, commission: f64) -> ClosedTradePayload {
        ClosedTradePayload {
            ticket: 1,
            symbol: symbol.to_owned(),
            kind: PositionKind::Buy,
            lots: 0.01,
            open_price: 1.0,
            close_price: 1.01,
            open_time: 1_700_000_000,
            close_time: 1_700_003_600,
            profit,
            swap,
            commission,
            magic: crate::broker::ORDER_MAGIC,
        }
    }

    #[test]
    fn an_empty_history_reports_zeroes_without_fake_ratios() {
        let report = summarize(&[]);
        assert_eq!(report.trades, 0);
        assert_eq!(report.win_rate_percent, 0.0);
        assert_eq!(report.net_profit, 0.0);
        assert_eq!(report.profit_factor, None);
        assert_eq!(report.average_win, None);
        assert_eq!(report.average_loss, None);
        assert_eq!(report.expectancy, None);
        assert_eq!(report.best_trade, None);
        assert_eq!(report.worst_trade, None);
        assert!(report.by_symbol.is_empty());
    }

    #[test]
    fn wins_losses_and_flats_are_counted_and_net_is_after_costs() {
        let mut winner = trade("USDJPY", 1.30, -0.01, 0.0);
        winner.ticket = 1;
        let mut loser = trade("EURUSD", -0.40, -0.02, 0.01);
        loser.ticket = 2;
        let mut flat = trade("GBPUSD", 0.0, -0.03, 0.03);
        flat.ticket = 3;
        let mut gold = trade("XAUUSD", 0.60, 0.05, 0.0);
        gold.ticket = 4;

        let report = summarize(&[winner, loser, flat, gold]);
        assert_eq!(report.trades, 4);
        assert_eq!(report.wins, 2);
        assert_eq!(report.losses, 1);
        assert_eq!(report.breakeven, 1);
        assert_eq!(report.win_rate_percent, 50.0);
        // Net: 1.29 - 0.41 + 0.00 + 0.65 = 1.53.
        assert_eq!(report.net_profit, 1.53);
        assert_eq!(report.gross_profit, 1.94);
        assert_eq!(report.gross_loss, 0.41);
        assert_eq!(report.profit_factor, Some(4.73));
        assert_eq!(report.average_win, Some(0.97));
        assert_eq!(report.average_loss, Some(0.41));
        assert_eq!(report.expectancy, Some(0.38));
        assert_eq!(report.best_trade, Some(1.29));
        assert_eq!(report.worst_trade, Some(-0.41));
    }

    #[test]
    fn symbol_totals_are_sorted_and_rounded() {
        let mut a = trade("EURUSD", 0.10, 0.0, 0.0);
        a.ticket = 1;
        let mut b = trade("EURUSD", -0.05, 0.0, 0.0);
        b.ticket = 2;
        let mut c = trade("USDJPY", 0.33, 0.0, 0.0);
        c.ticket = 3;
        let mut d = trade("XAUUSD", 0.20, 0.0, 0.0);
        d.ticket = 4;

        let report = summarize(&[a, b, c, d]);
        assert_eq!(report.by_symbol.len(), 3);
        assert_eq!(report.by_symbol[0].symbol, "EURUSD");
        assert_eq!(report.by_symbol[0].trades, 2);
        assert_eq!(report.by_symbol[0].wins, 1);
        assert_eq!(report.by_symbol[0].net_profit, 0.05);
        // The one-trade symbols sort by name when the count ties.
        assert_eq!(report.by_symbol[1].symbol, "USDJPY");
        assert_eq!(report.by_symbol[2].symbol, "XAUUSD");
    }

    #[test]
    fn an_unbeaten_streak_reports_no_profit_factor() {
        let mut a = trade("EURUSD", 0.25, 0.0, 0.0);
        a.ticket = 1;
        let report = summarize(&[a]);
        assert_eq!(report.wins, 1);
        assert_eq!(report.losses, 0);
        assert_eq!(report.profit_factor, None, "undefined without losses");
        assert_eq!(report.average_loss, None);
        assert_eq!(report.win_rate_percent, 100.0);
    }
}
