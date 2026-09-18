//! EA-backed market feed.
//!
//! Fetches closed candles from the MetaTrader 4 terminal by queueing one
//! read-only `rates` command on the broker control channel and waiting for its
//! acknowledgement. This is the first [`MarketFeed`] implementation: it owns
//! every EA wire type, so callers only ever see validated [`CandleSeries`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::broker::BrokerLink;
use crate::broker::Symbol;
use crate::broker::{
    CommandPayload, CommandState, RatesPayload, RatesRequest, SymbolSpecPayload, SymbolSpecRequest,
};
use crate::market::{
    Candle, CandleRequest, CandleSeries, MarketError, MarketFeed, MarketProvider, Timeframe,
};

/// Market feed served by the in-terminal EA over the command channel.
#[derive(Debug)]
pub struct EaMarketFeed {
    link: Arc<dyn BrokerLink>,
    await_timeout: Duration,
}

impl EaMarketFeed {
    /// Builds a feed over an existing EA link.
    pub fn new(link: Arc<dyn BrokerLink>, await_timeout: Duration) -> Self {
        Self {
            link,
            await_timeout,
        }
    }
}

#[async_trait]
impl MarketFeed for EaMarketFeed {
    fn provider(&self) -> MarketProvider {
        MarketProvider::Ea
    }

    async fn candles(&self, request: CandleRequest) -> Result<CandleSeries, MarketError> {
        let wire = RatesRequest::new(
            request.symbol(),
            request.timeframe().minutes(),
            request.bars(),
        )
        .map_err(|error| MarketError::InvalidRequest {
            reason: error.to_string(),
        })?;
        let id = self.link.enqueue_rates(wire);
        series_from_state(self.link.await_command(id, self.await_timeout).await)
    }

    async fn symbol_spec(&self, symbol: &Symbol) -> Result<SymbolSpecPayload, MarketError> {
        let wire = SymbolSpecRequest::new(symbol);
        let id = self.link.enqueue_symbol_spec(wire);
        spec_from_state(self.link.await_command(id, self.await_timeout).await)
    }
}

/// Maps a terminal command state to a domain series. Kept pure so every
/// outcome (including defensive ones the wire contract cannot produce) is
/// exercised by tests.
fn series_from_state(state: CommandState) -> Result<CandleSeries, MarketError> {
    match state {
        CommandState::Completed {
            payload: CommandPayload::Rates(payload),
        } => series_from_payload(&payload),
        CommandState::Completed { .. } => Err(MarketError::Contract {
            reason: "rates command completed with a different payload".to_owned(),
        }),
        CommandState::Failed { reason } => Err(MarketError::Unavailable { reason }),
        CommandState::Pending => Err(MarketError::Unavailable {
            reason: "rates command still pending after the await window".to_owned(),
        }),
    }
}

/// Maps a terminal command state to a validated instrument contract. Kept
/// pure so every outcome (including defensive ones the wire contract cannot
/// produce) is exercised by tests.
fn spec_from_state(state: CommandState) -> Result<SymbolSpecPayload, MarketError> {
    match state {
        CommandState::Completed {
            payload: CommandPayload::SymbolSpec(payload),
        } => {
            payload
                .validate()
                .map_err(|reason| MarketError::Contract { reason })?;
            Ok(payload)
        }
        CommandState::Completed { .. } => Err(MarketError::Contract {
            reason: "symbol_spec command completed with a different payload".to_owned(),
        }),
        CommandState::Failed { reason } => Err(MarketError::Unavailable { reason }),
        CommandState::Pending => Err(MarketError::Unavailable {
            reason: "symbol_spec command still pending after the await window".to_owned(),
        }),
    }
}

/// Parses a wire payload into domain candles; every invariant is re-checked
/// here so nothing can smuggle an unchecked series into strategy code even if
/// the payload was constructed by hand.
fn series_from_payload(payload: &RatesPayload) -> Result<CandleSeries, MarketError> {
    let contract = |reason: String| MarketError::Contract { reason };
    payload.validate().map_err(contract)?;
    let symbol = Symbol::parse(&payload.symbol)
        .map_err(|error| contract(format!("rates symbol is invalid: {error}")))?;
    let timeframe = Timeframe::from_minutes(payload.timeframe_minutes)
        .ok_or_else(|| contract("rates timeframe is not standard".to_owned()))?;
    let candles = payload
        .candles
        .iter()
        .map(|candle| {
            Candle::from_validated(
                candle.time,
                candle.open,
                candle.high,
                candle.low,
                candle.close,
                candle.volume,
            )
        })
        .collect();
    Ok(CandleSeries::from_validated(symbol, timeframe, candles))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::EaToken;
    use crate::broker::ea::create_ea_app;
    use crate::broker::{CandlePayload, EaLink};

    fn valid_payload() -> RatesPayload {
        RatesPayload {
            symbol: "EURUSD".to_owned(),
            timeframe_minutes: 240,
            candles: vec![
                CandlePayload {
                    time: 1_700_000_000,
                    open: 1.1,
                    high: 1.2,
                    low: 1.0,
                    close: 1.15,
                    volume: 42,
                },
                CandlePayload {
                    time: 1_700_014_400,
                    open: 1.15,
                    high: 1.3,
                    low: 1.1,
                    close: 1.25,
                    volume: 77,
                },
            ],
        }
    }

    fn valid_spec() -> SymbolSpecPayload {
        SymbolSpecPayload {
            symbol: "EURUSD".to_owned(),
            digits: 5,
            point: 0.00001,
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

    #[test]
    fn spec_payloads_convert_and_revalidate() {
        let spec = spec_from_state(CommandState::Completed {
            payload: CommandPayload::SymbolSpec(valid_spec()),
        })
        .expect("valid contract");
        assert_eq!(spec.symbol, "EURUSD");
        assert_eq!(spec.spread_points, 12);
        assert!((spec.margin_for(0.02) - 0.065_8).abs() < 1e-9);

        let mut broken = valid_spec();
        broken.margin_required = 0.0;
        let error = spec_from_state(CommandState::Completed {
            payload: CommandPayload::SymbolSpec(broken),
        })
        .expect_err("a hand-built payload is re-validated");
        assert!(error.to_string().contains("marginRequired"), "{error}");
    }

    #[test]
    fn spec_states_map_to_domain_outcomes() {
        let error = spec_from_state(CommandState::Completed {
            payload: CommandPayload::Ping,
        })
        .expect_err("non-spec payloads are contract violations");
        assert!(error.to_string().contains("different payload"), "{error}");

        let error = spec_from_state(CommandState::Failed {
            reason: "timeout".to_owned(),
        })
        .expect_err("failed commands are unavailable");
        assert!(error.to_string().contains("timeout"), "{error}");

        let error =
            spec_from_state(CommandState::Pending).expect_err("pending commands are unavailable");
        assert!(error.to_string().contains("pending"), "{error}");
    }

    #[test]
    fn payloads_convert_to_domain_series() {
        let series = series_from_payload(&valid_payload()).expect("valid series");
        assert_eq!(series.symbol().as_str(), "EURUSD");
        assert_eq!(series.timeframe(), Timeframe::H4);
        assert_eq!(series.candles().len(), 2);
        assert_eq!(series.last().expect("last").close(), 1.25);
        assert_eq!(series.candles()[0].volume(), 42);
    }

    #[test]
    fn conversion_rejects_contract_violations() {
        let mutate = |change: &dyn Fn(&mut RatesPayload)| {
            let mut payload = valid_payload();
            change(&mut payload);
            payload
        };

        let error = series_from_payload(&mutate(&|p| p.symbol = "EUR USD".to_owned()))
            .expect_err("invalid symbols are rejected");
        assert!(error.to_string().contains("symbol"), "{error}");

        let error = series_from_payload(&mutate(&|p| p.timeframe_minutes = 90))
            .expect_err("non-standard timeframes are rejected");
        assert!(error.to_string().contains("standard"), "{error}");

        let error = series_from_payload(&mutate(&|p| p.candles[0].high = 0.5))
            .expect_err("insane candles are rejected");
        assert!(error.to_string().contains("high"), "{error}");
    }

    #[test]
    fn states_map_to_domain_outcomes() {
        let series = series_from_state(CommandState::Completed {
            payload: CommandPayload::Rates(valid_payload()),
        })
        .expect("completed rates payloads convert");
        assert_eq!(series.candles().len(), 2);

        let error = series_from_state(CommandState::Completed {
            payload: CommandPayload::Ping,
        })
        .expect_err("non-rates payloads are contract violations");
        assert!(error.to_string().contains("different payload"), "{error}");

        let error = series_from_state(CommandState::Failed {
            reason: "timeout".to_owned(),
        })
        .expect_err("failed commands are unavailable");
        assert!(error.to_string().contains("timeout"), "{error}");

        let error =
            series_from_state(CommandState::Pending).expect_err("pending commands are unavailable");
        assert!(error.to_string().contains("pending"), "{error}");
    }

    #[actix_web::test]
    async fn feed_reports_unavailable_without_an_ack() {
        let link = Arc::new(EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(15),
        ));
        let feed = EaMarketFeed::new(link, Duration::from_millis(120));
        let request =
            CandleRequest::new(Symbol::parse("EURUSD").expect("symbol"), Timeframe::H4, 2)
                .expect("request");
        let error = feed
            .candles(request)
            .await
            .expect_err("an unanswered rates command must fail");
        assert!(error.to_string().contains("await timeout"), "{error}");
    }

    #[actix_web::test]
    async fn feed_round_trips_through_the_poll_channel() {
        let token = "test-token-1234567890";
        let link = Arc::new(EaLink::new(
            EaToken::parse(token).expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(15),
        ));
        let app = actix_web::test::init_service(create_ea_app(link.clone())).await;
        let feed = EaMarketFeed::new(link, Duration::from_secs(5));

        let request =
            CandleRequest::new(Symbol::parse("EURUSD").expect("symbol"), Timeframe::H4, 2)
                .expect("request");
        let feed_task = actix_web::rt::spawn(async move { feed.candles(request).await });

        // Poll until the queued rates command is handed to the terminal side.
        let hello = serde_json::json!({
            "t": "hb",
            "token": token,
            "acct": 94168,
            "server": "IFCMarkets-Real",
            "symbol": "EURUSD",
            "connected": true,
            "tradeAllowed": true,
            "orders": 0,
            "lots": 0.0
        });
        let mut command = None;
        for _ in 0..40 {
            let response = actix_web::test::call_service(
                &app,
                actix_web::test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            assert!(response.status().is_success());
            let body: serde_json::Value = actix_web::test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            actix_web::rt::time::sleep(Duration::from_millis(25)).await;
        }
        let command = command.expect("rates command delivered");
        assert_eq!(command["kind"], "rates");
        assert_eq!(command["rates"]["symbol"], "EURUSD");
        assert_eq!(command["rates"]["timeframeMinutes"], 240);
        assert_eq!(command["rates"]["bars"], 2);

        // Acknowledge with a valid series; the waiting feed converts it.
        let ack = serde_json::json!({
            "t": "ack",
            "token": token,
            "id": command["id"],
            "ok": true,
            "data": {
                "symbol": "EURUSD",
                "timeframeMinutes": 240,
                "candles": [
                    {"time": 1_700_000_000, "open": 1.1, "high": 1.2, "low": 1.0, "close": 1.15, "volume": 42},
                    {"time": 1_700_014_400, "open": 1.15, "high": 1.3, "low": 1.1, "close": 1.25, "volume": 77}
                ]
            }
        });
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(ack.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());

        let series = feed_task.await.expect("task joins").expect("series");
        assert_eq!(series.symbol().as_str(), "EURUSD");
        assert_eq!(series.timeframe(), Timeframe::H4);
        assert_eq!(series.candles().len(), 2);
        assert_eq!(series.candles()[1].close(), 1.25);
    }

    #[actix_web::test]
    async fn feed_round_trips_symbol_specs_through_the_poll_channel() {
        let token = "test-token-1234567890";
        let link = Arc::new(EaLink::new(
            EaToken::parse(token).expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(15),
        ));
        let app = actix_web::test::init_service(create_ea_app(link.clone())).await;
        let feed = EaMarketFeed::new(link, Duration::from_secs(5));

        let symbol = Symbol::parse("EURUSD").expect("symbol");
        let spec_task = actix_web::rt::spawn(async move { feed.symbol_spec(&symbol).await });

        let hello = serde_json::json!({
            "t": "hb",
            "token": token,
            "acct": 94168,
            "server": "IFCMarkets-Real",
            "symbol": "EURUSD",
            "connected": true,
            "tradeAllowed": true,
            "orders": 0,
            "lots": 0.0
        });
        let mut command = None;
        for _ in 0..40 {
            let response = actix_web::test::call_service(
                &app,
                actix_web::test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            assert!(response.status().is_success());
            let body: serde_json::Value = actix_web::test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            actix_web::rt::time::sleep(Duration::from_millis(25)).await;
        }
        let command = command.expect("symbol_spec command delivered");
        assert_eq!(command["kind"], "symbol_spec");
        assert_eq!(command["spec"]["symbol"], "EURUSD");

        let ack = serde_json::json!({
            "t": "ack",
            "token": token,
            "id": command["id"],
            "ok": true,
            "data": {
                "symbol": "EURUSD",
                "digits": 5,
                "point": 0.00001,
                "spreadPoints": 12,
                "stopLevelPoints": 5,
                "freezeLevelPoints": 0,
                "lotMin": 0.01,
                "lotMax": 100.0,
                "lotStep": 0.01,
                "tickValue": 0.1,
                "tickSize": 0.00001,
                "marginRequired": 3.29,
                "swapLong": -0.72,
                "swapShort": -0.31,
                "swapType": 0,
                "tradeAllowed": true
            }
        });
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(ack.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());

        let spec = spec_task.await.expect("task joins").expect("spec");
        assert_eq!(spec.symbol, "EURUSD");
        assert_eq!(spec.lot_min, 0.01);
        assert!(spec.trade_allowed);
    }
}
