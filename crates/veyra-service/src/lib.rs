//! Veyra's read-only control plane; no model calls and no trading authority.
//! Settings are parsed before listening, integrations are constructed once at
//! startup, and server completion is always awaited.

#![deny(missing_docs)]

pub mod app;
pub mod broker;
pub mod config;
pub mod observability;
pub mod routes;
pub mod server;

use broker::BrokerRuntime;
use config::ServiceConfig;

/// Immutable runtime state shared by HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    config: ServiceConfig,
    broker: Option<BrokerRuntime>,
}

impl AppState {
    /// Accepts already parsed and validated startup settings.
    pub fn new(config: ServiceConfig, broker: Option<BrokerRuntime>) -> Self {
        Self { config, broker }
    }

    /// Returns read-only settings without rereading the process environment.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Returns the active broker integration, if one is configured.
    pub fn broker(&self) -> Option<&BrokerRuntime> {
        self.broker.as_ref()
    }
}
