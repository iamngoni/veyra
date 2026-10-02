use std::sync::Arc;

use super::*;
use crate::broker::PositionKind;

pub(crate) fn trade(ticket: i64, close_time: i64, net: f64) -> ClosedTradePayload {
    ClosedTradePayload {
        ticket,
        symbol: "EURUSD".to_owned(),
        kind: PositionKind::Sell,
        lots: 0.01,
        open_price: 1.1,
        close_price: 1.099,
        open_time: close_time - 3_600,
        close_time,
        profit: net,
        swap: 0.0,
        commission: 0.0,
        magic: ORDER_MAGIC,
    }
}

fn config() -> crate::config::ServiceConfig {
    crate::config::ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("config")
}

fn state(ledger: Option<SharedLedger>, now: i64) -> AppState {
    AppState::new(
        config(),
        None,
        None,
        crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
    )
    .with_ledger(ledger)
    .with_fixed_now(Some(
        std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(now).expect("now")),
    ))
}

#[actix_web::test]
async fn the_memory_ledger_upserts_by_ticket_and_filters_by_window_and_magic() {
    let ledger = MemoryLedger::default();
    let mut other = trade(4, 5_000, 1.0);
    other.magic = 1;
    assert_eq!(
        ledger
            .record_trades(&[trade(1, 1_000, 1.0), trade(2, 3_000, -1.0), other])
            .await
            .expect("recorded"),
        3
    );
    // A second answer for the same ticket replaces it.
    ledger
        .record_trades(&[trade(2, 3_000, -2.0)])
        .await
        .expect("upsert");
    let found = ledger.closed_since(2_000, ORDER_MAGIC).await.expect("read");
    assert_eq!(
        found.len(),
        1,
        "older and foreign-magic trades are excluded"
    );
    assert_eq!(found[0].ticket, 2);
    assert!((found[0].profit + 2.0).abs() < 1e-9);
    let all = ledger.closed_since(0, ORDER_MAGIC).await.expect("read");
    assert_eq!(
        all.iter().map(|trade| trade.ticket).collect::<Vec<_>>(),
        vec![2, 1],
        "newest first"
    );
    assert_eq!(ledger.record_trades(&[]).await.expect("empty"), 0);
}

#[actix_web::test]
async fn without_a_terminal_the_ledger_still_answers() {
    let now = 10 * 86_400;
    let ledger: SharedLedger = Arc::new(MemoryLedger::default());
    ledger
        .record_trades(&[
            trade(1, now - 86_400, 0.5),
            trade(2, now - 40 * 86_400, 9.0),
        ])
        .await
        .expect("seeded");
    let history = closed_trades(&state(Some(ledger), now), 30)
        .await
        .expect("answered");
    assert_eq!(history.source, Source::LedgerOnly);
    assert_eq!(
        history.terminal_error.as_deref(),
        Some("broker unavailable")
    );
    assert_eq!(history.total, 1, "only the 30-day window");
    assert_eq!(history.orders[0].ticket, 1);
    assert_eq!(history.payload().orders.len(), 1);
}

#[actix_web::test]
async fn invalid_windows_and_nothing_to_ask_are_errors() {
    let bare = state(None, 1_000_000);
    assert_eq!(
        closed_trades(&bare, 0).await.expect_err("window"),
        HistoryError::InvalidWindow
    );
    assert_eq!(
        closed_trades(&bare, 366).await.expect_err("window"),
        HistoryError::InvalidWindow
    );
    assert_eq!(
        closed_trades(&bare, 30).await.expect_err("nothing"),
        HistoryError::Unavailable
    );
    assert_eq!(HistoryError::Failed("x".to_owned()).to_string(), "x");
}

#[actix_web::test]
async fn sync_does_nothing_without_a_ledger_or_broker() {
    // Returns at once instead of looping.
    sync_forever(state(None, 1_000)).await;
    sync_forever(state(Some(Arc::new(MemoryLedger::default())), 1_000)).await;
}

fn adjustment(ticket: i64, time: i64, amount: f64, comment: &str) -> BalanceOperationPayload {
    BalanceOperationPayload {
        ticket,
        kind: crate::broker::BalanceOperationKind::Balance,
        amount,
        time,
        comment: comment.to_owned(),
    }
}

#[actix_web::test]
async fn the_memory_ledger_keeps_balance_operations_by_ticket() {
    let ledger = MemoryLedger::default();
    assert_eq!(
        ledger
            .record_adjustments(&[
                adjustment(1, 1_000, -0.12, "Dividend US500"),
                adjustment(2, 3_000, 50.0, "Deposit"),
            ])
            .await
            .expect("recorded"),
        2
    );
    // A later answer for the same ticket replaces it.
    ledger
        .record_adjustments(&[adjustment(2, 3_000, 60.0, "Deposit")])
        .await
        .expect("upsert");
    let recent = ledger.adjustments_since(2_000).await.expect("read");
    assert_eq!(recent.len(), 1);
    assert!((recent[0].amount - 60.0).abs() < 1e-9);
    let all = ledger.adjustments_since(0).await.expect("read");
    assert_eq!(
        all.iter().map(|entry| entry.ticket).collect::<Vec<_>>(),
        vec![2, 1],
        "newest first"
    );
}

#[test]
fn adjustments_are_summed_by_category() {
    let mut credit = adjustment(4, 4, 10.0, "bonus");
    credit.kind = crate::broker::BalanceOperationKind::Credit;
    let summary = AdjustmentSummary::of(&[
        adjustment(1, 1, -0.12, "Dividend US500"),
        adjustment(2, 2, -0.08, "div adj NDX"),
        adjustment(3, 3, 0.05, "correction"),
        adjustment(5, 5, 100.0, "Deposit via card"),
        adjustment(6, 6, -20.0, "Withdrawal"),
        adjustment(7, 7, 20.0, "D828081/BB/BTC"),
        adjustment(8, 8, -5.0, "W123456"),
        adjustment(9, 9, 1.0, "D12 fee"),
        credit,
    ]);
    assert_eq!(summary.count, 9);
    assert!((summary.dividends + 0.20).abs() < 1e-9);
    // A short `D12` is not a back-office reference.
    assert!((summary.other - 1.05).abs() < 1e-9);
    assert!((summary.transfers - 95.0).abs() < 1e-9);
    assert!((summary.credit - 10.0).abs() < 1e-9);
    assert_eq!(AdjustmentSummary::of(&[]), AdjustmentSummary::default());
}

#[actix_web::test]
async fn the_ledger_answers_with_its_balance_operations() {
    let now = 10 * 86_400;
    let ledger: SharedLedger = Arc::new(MemoryLedger::default());
    ledger
        .record_adjustments(&[
            adjustment(1, now - 86_400, -0.12, "Dividend"),
            adjustment(2, now - 40 * 86_400, 5.0, "old"),
        ])
        .await
        .expect("seeded");
    let history = closed_trades(&state(Some(ledger), now), 30)
        .await
        .expect("answered");
    assert_eq!(history.adjustments.len(), 1, "only the 30-day window");
    assert_eq!(history.payload().adjustments.len(), 1);
}
