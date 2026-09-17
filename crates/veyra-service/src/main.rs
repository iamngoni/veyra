//! Minimal process wiring: fail before listening if configuration, logging, or
//! broker settings are invalid, then serve the diagnostic surface plus any
//! provider-required loopback listener. This binary has no execution path.

use veyra_service::broker::{BrokerRuntime, BrokerSettings};
use veyra_service::jev::{JevRuntime, JevSettings};
use veyra_service::model::{ModelRuntime, settings::ModelSettings};
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
    let app = server::build_server(
        AppState::new(config, broker, model, risk).with_jev(jev),
        listener,
    )?;
    server::serve(app, companion).await?;
    Ok(())
}
