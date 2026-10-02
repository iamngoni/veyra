//! Veyra's own record of closed trades, and the one way every surface reads
//! them.
//!
//! The terminal only reveals the account history its "Account History" tab is
//! set to show, so asking it alone made performance depend on a UI filter, a
//! restart, or the terminal being online. Here every history answer is
//! upserted into a durable [`TradeLedger`] (one row per ticket, never pruned
//! with the audit trail), and reads come from the ledger:
//!
//! * [`closed_trades`] asks the terminal, records what it returns, then
//!   answers from the ledger for the requested window. When the terminal does
//!   not answer, the ledger still does (the result says so).
//! * [`sync_forever`] keeps the ledger current without anyone opening the
//!   console: a full-year backfill once, then a week every 15 minutes.
//!
//! Read-only towards the venue: the only command is the existing
//! `order_history` for the Veyra magic number.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::AppState;
use crate::broker::{
    AdjustmentCategory, BalanceOperationPayload, ClosedTradePayload, CommandPayload, CommandState,
    ORDER_MAGIC, OrderHistoryPayload, OrderHistoryRequest,
};

/// How long one history request may wait for the terminal.
pub const HISTORY_WAIT: Duration = Duration::from_secs(20);

/// How often [`sync_forever`] refreshes recent history.
pub const SYNC_PERIOD: Duration = Duration::from_secs(15 * 60);

/// Days each periodic sync asks for.
pub const SYNC_DAYS: u32 = 7;

/// Days the first sync asks for.
pub const BACKFILL_DAYS: u32 = 365;

/// Why the ledger could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("trade ledger failed: {reason}")]
pub struct LedgerError {
    /// Non-sensitive explanation.
    pub reason: String,
}

/// Durable store of closed trades, keyed by ticket.
#[async_trait]
pub trait TradeLedger: Send + Sync + fmt::Debug + 'static {
    /// Inserts or refreshes every trade; returns how many rows were written.
    ///
    /// # Errors
    /// Returns [`LedgerError`] when the store is unavailable.
    async fn record_trades(&self, trades: &[ClosedTradePayload]) -> Result<usize, LedgerError>;

    /// Trades with `magic` closed at or after `since` (broker server seconds),
    /// newest first.
    ///
    /// # Errors
    /// Returns [`LedgerError`] when the store is unavailable.
    async fn closed_since(
        &self,
        since: i64,
        magic: u32,
    ) -> Result<Vec<ClosedTradePayload>, LedgerError>;

    /// Inserts or refreshes balance operations and credit entries; returns
    /// how many rows were written.
    ///
    /// # Errors
    /// Returns [`LedgerError`] when the store is unavailable.
    async fn record_adjustments(
        &self,
        adjustments: &[BalanceOperationPayload],
    ) -> Result<usize, LedgerError>;

    /// Balance operations booked at or after `since` (broker server
    /// seconds), newest first.
    ///
    /// # Errors
    /// Returns [`LedgerError`] when the store is unavailable.
    async fn adjustments_since(
        &self,
        since: i64,
    ) -> Result<Vec<BalanceOperationPayload>, LedgerError>;
}

/// In-memory ledger for tests and database-less runs.
#[derive(Debug, Default)]
pub struct MemoryLedger {
    trades: Mutex<std::collections::BTreeMap<i64, ClosedTradePayload>>,
    adjustments: Mutex<std::collections::BTreeMap<i64, BalanceOperationPayload>>,
}

#[async_trait]
impl TradeLedger for MemoryLedger {
    async fn record_trades(&self, trades: &[ClosedTradePayload]) -> Result<usize, LedgerError> {
        let mut stored = self.trades.lock().map_err(|_| LedgerError {
            reason: "memory ledger lock poisoned".to_owned(),
        })?;
        for trade in trades {
            stored.insert(trade.ticket, trade.clone());
        }
        Ok(trades.len())
    }

    async fn closed_since(
        &self,
        since: i64,
        magic: u32,
    ) -> Result<Vec<ClosedTradePayload>, LedgerError> {
        let stored = self.trades.lock().map_err(|_| LedgerError {
            reason: "memory ledger lock poisoned".to_owned(),
        })?;
        let mut trades: Vec<ClosedTradePayload> = stored
            .values()
            .filter(|trade| trade.magic == magic && trade.close_time >= since)
            .cloned()
            .collect();
        newest_first(&mut trades);
        Ok(trades)
    }

    async fn record_adjustments(
        &self,
        adjustments: &[BalanceOperationPayload],
    ) -> Result<usize, LedgerError> {
        let mut stored = self.adjustments.lock().map_err(|_| LedgerError {
            reason: "memory ledger lock poisoned".to_owned(),
        })?;
        for adjustment in adjustments {
            stored.insert(adjustment.ticket, adjustment.clone());
        }
        Ok(adjustments.len())
    }

    async fn adjustments_since(
        &self,
        since: i64,
    ) -> Result<Vec<BalanceOperationPayload>, LedgerError> {
        let stored = self.adjustments.lock().map_err(|_| LedgerError {
            reason: "memory ledger lock poisoned".to_owned(),
        })?;
        let mut adjustments: Vec<BalanceOperationPayload> = stored
            .values()
            .filter(|adjustment| adjustment.time >= since)
            .cloned()
            .collect();
        newest_adjustments_first(&mut adjustments);
        Ok(adjustments)
    }
}

/// Sorts balance operations newest first, ties by ticket.
pub fn newest_adjustments_first(adjustments: &mut [BalanceOperationPayload]) {
    adjustments.sort_by(|left, right| {
        right
            .time
            .cmp(&left.time)
            .then(right.ticket.cmp(&left.ticket))
    });
}

/// Totals of balance operations by category, for reporting beside trade P/L.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjustmentSummary {
    /// Entries in the window.
    pub count: usize,
    /// Dividend adjustments (index and share CFDs).
    pub dividends: f64,
    /// Other corrections the broker booked.
    pub other: f64,
    /// Deposits and withdrawals: the holder's money, not performance.
    pub transfers: f64,
    /// Broker credit: not the holder's money.
    pub credit: f64,
}

impl AdjustmentSummary {
    /// Sums `adjustments` by [`AdjustmentCategory`].
    pub fn of(adjustments: &[BalanceOperationPayload]) -> Self {
        let mut summary = Self {
            count: adjustments.len(),
            ..Self::default()
        };
        for adjustment in adjustments {
            let bucket = match adjustment.category() {
                AdjustmentCategory::Dividend => &mut summary.dividends,
                AdjustmentCategory::Adjustment => &mut summary.other,
                AdjustmentCategory::Transfer => &mut summary.transfers,
                AdjustmentCategory::Credit => &mut summary.credit,
            };
            *bucket += adjustment.amount;
        }
        summary
    }
}

/// Sorts trades newest close first, ties by ticket.
pub fn newest_first(trades: &mut [ClosedTradePayload]) {
    trades.sort_by(|left, right| {
        right
            .close_time
            .cmp(&left.close_time)
            .then(right.ticket.cmp(&left.ticket))
    });
}

/// Where a [`History`] answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Veyra's ledger, refreshed by the terminal's answer just now.
    Ledger,
    /// Veyra's ledger only: the terminal did not answer this time.
    LedgerOnly,
    /// The terminal alone (no ledger is configured).
    Terminal,
}

/// Closed trades for one window.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    /// Closed Veyra trades, newest first.
    pub orders: Vec<ClosedTradePayload>,
    /// Trades in the window (equals `orders.len()` when read from the ledger).
    pub total: u32,
    /// Whether the terminal's own answer was cut at its cap (the ledger keeps
    /// what earlier answers held).
    pub truncated: bool,
    /// Where the answer came from.
    pub source: Source,
    /// Why the terminal did not answer, when it did not.
    pub terminal_error: Option<String>,
    /// Balance operations and credit in the window, newest first.
    pub adjustments: Vec<BalanceOperationPayload>,
}

impl History {
    /// The answer in the terminal's payload shape, for existing consumers.
    pub fn payload(&self) -> OrderHistoryPayload {
        OrderHistoryPayload {
            orders: self.orders.clone(),
            total: self.total,
            truncated: self.truncated,
            adjustments: self.adjustments.clone(),
        }
    }
}

/// Why closed trades could not be read at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    /// The window is outside 1-365 days.
    #[error("invalid history window: days must be from 1 through 365")]
    InvalidWindow,
    /// No broker and no ledger.
    #[error("broker unavailable")]
    Unavailable,
    /// The terminal failed and there is no ledger to answer instead.
    #[error("{0}")]
    Failed(String),
}

/// Asks the terminal for `days` of account history.
async fn from_terminal(
    state: &AppState,
    request: OrderHistoryRequest,
) -> Option<Result<OrderHistoryPayload, String>> {
    let broker = state.broker()?;
    let link = broker.link();
    let id = link.enqueue_order_history(request);
    Some(match link.await_command(id, HISTORY_WAIT).await {
        CommandState::Completed {
            payload: CommandPayload::OrderHistory(history),
        } => Ok(history),
        CommandState::Completed { .. } => {
            Err("history command completed with a different payload".to_owned())
        }
        CommandState::Failed { reason } => Err(reason),
        CommandState::Pending => {
            Err("history command still pending after the await window".to_owned())
        }
    })
}

/// The broker server's current time, from the retained account snapshot's
/// clock offset; UTC when the offset is not known yet.
fn broker_now(state: &AppState) -> i64 {
    let utc = state
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let offset = crate::broker_clock::BrokerClock::from_state(state)
        .map(crate::broker_clock::BrokerClock::offset_secs)
        .unwrap_or(0);
    utc.saturating_add(offset)
}

/// Closed Veyra trades for the last `days` days: asks the terminal, records
/// its answer in the ledger, and answers from the ledger (see the module
/// notes). Without a ledger this is the terminal's answer, as before.
///
/// # Errors
/// Returns [`HistoryError`] for an invalid window, or when neither the
/// terminal nor the ledger can answer.
pub async fn closed_trades(state: &AppState, days: u32) -> Result<History, HistoryError> {
    let request =
        OrderHistoryRequest::new(days, ORDER_MAGIC).map_err(|_| HistoryError::InvalidWindow)?;
    let terminal = from_terminal(state, request).await;
    let Some(ledger) = state.ledger() else {
        return match terminal {
            None => Err(HistoryError::Unavailable),
            Some(Ok(history)) => Ok(History {
                total: history.total,
                truncated: history.truncated,
                orders: history.orders,
                source: Source::Terminal,
                terminal_error: None,
                adjustments: history.adjustments,
            }),
            Some(Err(reason)) => Err(HistoryError::Failed(reason)),
        };
    };
    let (truncated, terminal_error) = match &terminal {
        Some(Ok(history)) => {
            if let Err(error) = ledger.record_trades(&history.orders).await {
                tracing::warn!(%error, "closed trades could not be recorded");
            }
            if let Err(error) = ledger.record_adjustments(&history.adjustments).await {
                tracing::warn!(%error, "balance operations could not be recorded");
            }
            (history.truncated, None)
        }
        Some(Err(reason)) => (false, Some(reason.clone())),
        None => (false, Some("broker unavailable".to_owned())),
    };
    let since = broker_now(state).saturating_sub(i64::from(days) * 86_400);
    // Balance operations are reporting only: an unreadable table falls back to
    // what the terminal said this time rather than failing the trades.
    let adjustments = match ledger.adjustments_since(since).await {
        Ok(adjustments) => adjustments,
        Err(error) => {
            tracing::warn!(%error, "balance operations unreadable; using the terminal's");
            let mut adjustments = match &terminal {
                Some(Ok(history)) => history.adjustments.clone(),
                _ => Vec::new(),
            };
            newest_adjustments_first(&mut adjustments);
            adjustments
        }
    };
    match ledger.closed_since(since, ORDER_MAGIC).await {
        Ok(orders) => Ok(History {
            total: u32::try_from(orders.len()).unwrap_or(u32::MAX),
            orders,
            truncated,
            source: if terminal_error.is_none() {
                Source::Ledger
            } else {
                Source::LedgerOnly
            },
            terminal_error,
            adjustments,
        }),
        Err(error) => {
            tracing::warn!(%error, "trade ledger unreadable; answering from the terminal");
            match terminal {
                Some(Ok(mut history)) => {
                    newest_first(&mut history.orders);
                    Ok(History {
                        total: history.total,
                        truncated: history.truncated,
                        orders: history.orders,
                        source: Source::Terminal,
                        terminal_error: None,
                        adjustments,
                    })
                }
                Some(Err(reason)) => Err(HistoryError::Failed(reason)),
                None => Err(HistoryError::Unavailable),
            }
        }
    }
}

/// Keeps the ledger current for the life of the process: one
/// [`BACKFILL_DAYS`] sync (retried every [`SYNC_PERIOD`] until the terminal
/// answers), then [`SYNC_DAYS`] every [`SYNC_PERIOD`].
pub async fn sync_forever(state: AppState) {
    if state.ledger().is_none() || state.broker().is_none() {
        return;
    }
    let mut backfilled = false;
    loop {
        let days = if backfilled { SYNC_DAYS } else { BACKFILL_DAYS };
        match closed_trades(&state, days).await {
            Ok(history) if history.terminal_error.is_none() => {
                if !backfilled {
                    tracing::info!(trades = history.orders.len(), "trade ledger backfilled");
                }
                backfilled = true;
            }
            Ok(history) => tracing::debug!(
                reason = history.terminal_error.as_deref().unwrap_or(""),
                "trade ledger sync skipped; terminal did not answer"
            ),
            Err(error) => tracing::debug!(%error, "trade ledger sync skipped"),
        }
        actix_web::rt::time::sleep(SYNC_PERIOD).await;
    }
}

/// A shareable ledger handle.
pub type SharedLedger = Arc<dyn TradeLedger>;

#[cfg(test)]
mod tests;
