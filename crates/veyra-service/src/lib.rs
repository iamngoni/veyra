//! Veyra's read-only control plane; no broker, model calls, or trading authority.
//! Settings are parsed before listening, and server completion is always awaited.

#![deny(missing_docs)]

pub mod app;
pub mod config;
pub mod observability;
pub mod routes;
pub mod server;

use config::ServiceConfig;

/// Immutable runtime state shared by diagnostic handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    config: ServiceConfig,
}

impl AppState {
    /// Accepts already parsed startup settings.
    pub fn new(config: ServiceConfig) -> Self {
        Self { config }
    }

    /// Returns read-only settings without rereading the process environment.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }
}
