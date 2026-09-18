//! Veyra's non-executing control plane: diagnostics plus deterministic risk
//! evaluation. Settings are parsed before listening, integrations are
//! constructed once at startup, and server completion is always awaited. No
//! module here can place, modify, or cancel an order.

#![deny(missing_docs)]

pub mod app;
pub mod audit;
pub mod broker;
pub mod config;
pub mod control;
pub mod jev;
pub mod logs;
pub mod market;
pub mod model;
pub mod observability;
pub mod reconciliation;
pub mod risk;
pub mod routes;
pub mod server;
pub mod store;
pub mod trading;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use audit::AuditRuntime;
use broker::BrokerRuntime;
use config::ServiceConfig;
use jev::JevRuntime;
use logs::LogBuffer;
use market::MarketRuntime;
use model::ModelRuntime;
use risk::RiskGate;
use risk::guard::EquityGuard;
use trading::autopilot::{AutopilotSettings, StopBasis};

/// Immutable runtime state shared by HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    config: ServiceConfig,
    broker: Option<BrokerRuntime>,
    market: Option<MarketRuntime>,
    autopilot: Option<AutopilotSettings>,
    model: Option<ModelRuntime>,
    jev: Option<JevRuntime>,
    audit: Option<AuditRuntime>,
    logs: Option<Arc<LogBuffer>>,
    risk: RiskGate,
    stop_basis: Arc<StopBasis>,
    rotation: Arc<AtomicUsize>,
    equity_guard: Arc<EquityGuard>,
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
            market: None,
            autopilot: None,
            model,
            jev: None,
            audit: None,
            logs: None,
            risk,
            stop_basis: Arc::new(StopBasis::default()),
            rotation: Arc::new(AtomicUsize::new(0)),
            equity_guard: Arc::new(EquityGuard::new()),
        }
    }

    /// Attaches the configured market-data integration, if any.
    pub fn with_market(mut self, market: Option<MarketRuntime>) -> Self {
        self.market = market;
        self
    }

    /// Attaches the configured autonomous loop settings, if any.
    pub fn with_autopilot(mut self, autopilot: Option<AutopilotSettings>) -> Self {
        self.autopilot = autopilot;
        self
    }

    /// Attaches the configured audit trail, if any.
    pub fn with_audit(mut self, audit: Option<AuditRuntime>) -> Self {
        self.audit = audit;
        self
    }

    /// Attaches the configured judgement integration, if any.
    pub fn with_jev(mut self, jev: Option<JevRuntime>) -> Self {
        self.jev = jev;
        self
    }

    /// Attaches the in-process log buffer shown by the console, if any.
    pub fn with_logs(mut self, logs: Arc<LogBuffer>) -> Self {
        self.logs = Some(logs);
        self
    }

    /// Returns read-only settings without rereading the process environment.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Returns the active broker integration, if one is configured.
    pub fn broker(&self) -> Option<&BrokerRuntime> {
        self.broker.as_ref()
    }

    /// Returns the active market-data integration, if one is configured.
    pub fn market(&self) -> Option<&MarketRuntime> {
        self.market.as_ref()
    }

    /// Returns the autonomous loop settings, if any were configured.
    pub fn autopilot(&self) -> Option<&AutopilotSettings> {
        self.autopilot.as_ref()
    }

    /// Entry-risk memory shared by the autopilot's stop policies; survives
    /// across ticks for the lifetime of the process.
    pub fn stop_basis(&self) -> &Arc<StopBasis> {
        &self.stop_basis
    }

    /// Returns the active model integration, if one is configured.
    pub fn model(&self) -> Option<&ModelRuntime> {
        self.model.as_ref()
    }

    /// Returns the active judgement integration, if one is configured.
    pub fn jev(&self) -> Option<&JevRuntime> {
        self.jev.as_ref()
    }

    /// Returns the active audit trail, if one is configured.
    pub fn audit(&self) -> Option<&AuditRuntime> {
        self.audit.as_ref()
    }

    /// Returns the in-process log buffer shown by the console, if any.
    pub fn logs(&self) -> Option<&Arc<LogBuffer>> {
        self.logs.as_ref()
    }

    /// Returns the deterministic risk gate every intent must pass.
    pub fn risk(&self) -> &RiskGate {
        &self.risk
    }

    /// Cursor the multi-symbol autopilot advances once per tick so each
    /// configured instrument gets its turn.
    pub fn rotation(&self) -> &Arc<AtomicUsize> {
        &self.rotation
    }

    /// Equity baseline tracker feeding the daily and peak drawdown breakers.
    pub fn equity_guard(&self) -> &Arc<EquityGuard> {
        &self.equity_guard
    }
}
