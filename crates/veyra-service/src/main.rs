//! Minimal process wiring: fail before listening if configuration, logging, or
//! broker settings are invalid, then serve the diagnostic surface plus any
//! provider-required loopback listener. This binary has no execution path.

use std::time::Duration;

use veyra_service::broker::{BrokerRuntime, BrokerSettings};
use veyra_service::jev::{JevRuntime, JevSettings};
use veyra_service::model::{ModelRuntime, settings::ModelSettings};
use veyra_service::reconciliation;
use veyra_service::risk::{RiskGate, RiskPolicy};
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

    let companion = match &broker {
        Some(runtime) => runtime.listener()?,
        None => None,
    };

    // The gate is always present and restrictive by default: an unconfigured
    // allowlist approves nothing, so a missing setting cannot widen behavior.
    let risk = RiskGate::new(RiskPolicy::from_env()?);

    let listener = server::bind(&config)?;
    let state = AppState::new(config, broker, model, risk).with_jev(jev);

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

    let app = server::build_server(state, listener)?;
    server::serve(app, companion).await?;
    Ok(())
}
