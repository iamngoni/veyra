//! Minimal process wiring: fail before listening if configuration, logging, or
//! broker settings are invalid, then serve the diagnostic surface plus any
//! provider-required loopback listener. This binary has no execution path.

use std::sync::Arc;
use std::time::Duration;

use veyra_service::audit::{AuditEvent, AuditKind, AuditRuntime};
use veyra_service::broker::{BrokerRuntime, BrokerSettings};
use veyra_service::jev::{JevRuntime, JevSettings};
use veyra_service::market::{MarketRuntime, MarketSettings};
use veyra_service::model::{ModelRuntime, settings::ModelSettings};
use veyra_service::reconciliation;
use veyra_service::risk::{RiskGate, RiskPolicy};
use veyra_service::store::Store;
use veyra_service::trading::autopilot::{AutopilotSettings, TickOutcome};
use veyra_service::{AppState, config::ServiceConfig, observability, server};

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServiceConfig::from_env()?;
    observability::init()?;

    let broker = match BrokerSettings::from_env()? {
        Some(settings) => Some(BrokerRuntime::from_settings(settings)?),
        None => None,
    };
    let model = match ModelSettings::from_env()? {
        Some(settings) => Some(ModelRuntime::from_settings(settings)?),
        None => None,
    };

    let jev = match JevSettings::from_env()? {
        Some(settings) => Some(JevRuntime::from_settings(settings)?),
        None => None,
    };

    // Market data follows the broker: the EA provider reads candles through
    // the same command channel, so it refuses to build without it.
    let market = match MarketSettings::from_env()? {
        Some(settings) => Some(MarketRuntime::from_settings(settings, broker.as_ref())?),
        None => None,
    };

    let autopilot = AutopilotSettings::from_env()?;

    let companion = match &broker {
        Some(runtime) => runtime.listener()?,
        None => None,
    };

    // The gate is always present and restrictive by default: an unconfigured
    // allowlist approves nothing, so a missing setting cannot widen behavior.
    let risk = RiskGate::new(RiskPolicy::from_env()?);

    // A configured database must be reachable before the service listens: a
    // missing audit trail is never mistaken for an empty one. Writes are
    // best-effort once running.
    let audit = match config.database_url() {
        Some(url) => {
            let store = Store::connect(url).await?;
            store.migrate().await?;
            Some(Arc::new(AuditRuntime::new(Arc::new(store))))
        }
        None => None,
    };

    let listener = server::bind(&config)?;
    let state = AppState::new(config, broker, model, risk)
        .with_market(market)
        .with_autopilot(autopilot)
        .with_jev(jev)
        .with_audit(audit.as_ref().map(|runtime| (**runtime).clone()));

    if let Some(runtime) = &audit {
        if let Some(link) = state.broker().and_then(|broker| broker.ea_link()) {
            link.set_audit(runtime.clone());
        }
        runtime
            .try_record(AuditEvent::new(
                AuditKind::ServiceStarted,
                serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
            ))
            .await;
    }

    // Keep broker state fresh for the close/modify guards and the
    // reconciliation view while the terminal is polling.
    let refresh_secs = state.config().reconcile_secs();
    if refresh_secs > 0 {
        let refresh_state = state.clone();
        actix_web::rt::spawn(async move {
            let period = Duration::from_secs(refresh_secs);
            loop {
                actix_web::rt::time::sleep(period).await;
                if reconciliation::refresh_once(&refresh_state).await {
                    tracing::debug!("queued periodic account snapshot");
                }
            }
        });
    }

    // Autonomous loop: decide on a cadence, obeying the same gate and staged
    // execution as the control surface. The first tick runs one interval after
    // startup so a restart never fires immediately.
    if let Some(settings) = state.autopilot().filter(|settings| settings.enabled()) {
        let period = settings.interval();
        tracing::info!(?period, "autopilot loop enabled");
        let autopilot_state = state.clone();
        actix_web::rt::spawn(async move {
            loop {
                actix_web::rt::time::sleep(period).await;
                match veyra_service::trading::autopilot::tick(&autopilot_state).await {
                    TickOutcome::Skipped { reason } => {
                        tracing::debug!(reason, "autopilot tick skipped");
                    }
                    outcome => tracing::info!(?outcome, "autopilot tick"),
                }
            }
        });
    }

    // Retention: prune audit history once an hour, best-effort. Zero days
    // keeps everything.
    let retention_days = state.config().audit_retention_days();
    if let (Some(runtime), true) = (state.audit(), retention_days > 0) {
        let retention = runtime.clone();
        actix_web::rt::spawn(async move {
            loop {
                actix_web::rt::time::sleep(Duration::from_secs(3_600)).await;
                let deleted = retention.try_prune(retention_days).await;
                if deleted > 0 {
                    tracing::info!(deleted, "pruned audit rows");
                }
            }
        });
    }

    let app = server::build_server(state, listener)?;
    server::serve(app, companion).await?;
    Ok(())
}
