//! Veyra's non-executing control plane: diagnostics plus deterministic risk
//! evaluation. Settings are parsed before listening, integrations are
//! constructed once at startup, and server completion is always awaited. No
//! module here can place, modify, or cancel an order.

#![deny(missing_docs)]

pub mod app;
pub mod broker;
pub mod config;
pub mod model;
pub mod observability;
pub mod risk;
pub mod routes;
pub mod server;
pub mod trading;

use broker::BrokerRuntime;
use config::ServiceConfig;
use model::ModelRuntime;
use risk::RiskGate;

/// Immutable runtime state shared by HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    config: ServiceConfig,
    broker: Option<BrokerRuntime>,
    model: Option<ModelRuntime>,
    risk: RiskGate,
}

impl AppState {
    /// Accepts already parsed and validated startup settings.
    pub fn new(
        config: ServiceConfig,
        broker: Option<BrokerRuntime>,
        model: Option<ModelRuntime>,
        risk: RiskGate,
    ) -> Self {
        Self {
            config,
            broker,
            model,
            risk,
        }
    }

    /// Returns read-only settings without rereading the process environment.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Returns the active broker integration, if one is configured.
    pub fn broker(&self) -> Option<&BrokerRuntime> {
        self.broker.as_ref()
    }

    /// Returns the active model integration, if one is configured.
    pub fn model(&self) -> Option<&ModelRuntime> {
        self.model.as_ref()
    }

    /// Returns the deterministic risk gate every intent must pass.
    pub fn risk(&self) -> &RiskGate {
        &self.risk
    }
}
