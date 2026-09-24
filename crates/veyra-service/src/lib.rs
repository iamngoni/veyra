//! Veyra's non-executing control plane: diagnostics plus deterministic risk
//! evaluation. Settings are parsed before listening, integrations are
//! constructed once at startup, and server completion is always awaited. No
//! module here can place, modify, or cancel an order.

#![deny(missing_docs)]

pub mod app;
pub mod assistant_chat;
pub mod audit;
pub mod balance;
pub mod broker;
pub mod calendar;
pub mod config;
pub mod control;
pub mod credential;
pub mod jev;
pub mod logs;
pub mod market;
pub mod model;
pub mod observability;
pub mod performance;
pub mod reconciliation;
pub mod risk;
pub mod routes;
pub mod runtime_config;
pub mod server;
pub mod state;
pub mod store;
pub mod subscription_auth;
pub mod trading;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use audit::AuditRuntime;
use broker::BrokerRuntime;
use calendar::CalendarRuntime;
use config::ServiceConfig;
use jev::JevRuntime;
use logs::LogBuffer;
use market::MarketRuntime;
use model::ModelRuntime;
use risk::RiskGate;
use risk::guard::EquityGuard;
use state::RuntimeState;
use trading::autopilot::{AutopilotSettings, ProfitHarvestBook, StopBasis};

/// Immutable runtime state shared by HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    config: ServiceConfig,
    broker: Option<BrokerRuntime>,
    market: Option<MarketRuntime>,
    calendar: Option<CalendarRuntime>,
    /// Live autopilot settings. Behind a lock because the console edits them
    /// without a restart; every reader takes a snapshot for the duration of a
    /// tick rather than holding the guard across an await.
    autopilot: Arc<std::sync::RwLock<Option<AutopilotSettings>>>,
    /// Live model integration, rebuilt in place when its settings or the
    /// ChatGPT subscription connection change.
    model: Arc<std::sync::RwLock<Option<ModelRuntime>>>,
    /// Per-candidate model cooldowns, shared by every engine rebuild.
    model_cooldowns: crate::model::CooldownRegistry,
    /// The service half of the execution control, overridable at runtime.
    /// `None` means "as configured at startup".
    trading_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Writable overlay in front of the environment for every live setting.
    runtime_config: crate::runtime_config::RuntimeConfig,
    credential_vault: Option<crate::credential::CredentialVault>,
    subscription_auth: crate::subscription_auth::SubscriptionAuthState,
    jev: Option<JevRuntime>,
    audit: Option<AuditRuntime>,
    logs: Option<Arc<LogBuffer>>,
    risk: RiskGate,
    runtime_state: RuntimeState,
    /// Pinned instant for deterministic tests; production reads the clock.
    fixed_now: Option<std::time::SystemTime>,
    stop_basis: Arc<StopBasis>,
    profit_harvest_book: Arc<ProfitHarvestBook>,
    entry_watch: Arc<crate::trading::autopilot::EntryWatch>,
    judgements: Arc<crate::trading::autopilot::JudgementCache>,
    review_watch: Arc<crate::trading::autopilot::ReviewWatch>,
    weekend_watch: Arc<crate::trading::autopilot::ReviewWatch>,
    decision_health: Arc<crate::trading::autopilot::DecisionHealth>,
    rotation: Arc<AtomicUsize>,
    equity_guard: Arc<EquityGuard>,
    order_admission: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    /// Accepts already parsed and validated startup settings.
    pub fn new(
        config: ServiceConfig,
        broker: Option<BrokerRuntime>,
        model: Option<ModelRuntime>,
        risk: RiskGate,
    ) -> Self {
        let trading_enabled =
            Arc::new(std::sync::atomic::AtomicBool::new(config.trading_enabled()));
        Self {
            config,
            broker,
            market: None,
            calendar: None,
            autopilot: Arc::new(std::sync::RwLock::new(None)),
            model: Arc::new(std::sync::RwLock::new(model)),
            model_cooldowns: crate::model::CooldownRegistry::new(),
            trading_enabled,
            runtime_config: crate::runtime_config::RuntimeConfig::new(),
            credential_vault: None,
            subscription_auth: crate::subscription_auth::SubscriptionAuthState::new(),
            jev: None,
            audit: None,
            logs: None,
            risk,
            runtime_state: RuntimeState::disabled(),
            fixed_now: None,
            stop_basis: Arc::new(StopBasis::default()),
            profit_harvest_book: Arc::new(ProfitHarvestBook::default()),
            entry_watch: Arc::new(crate::trading::autopilot::EntryWatch::default()),
            judgements: Arc::new(crate::trading::autopilot::JudgementCache::default()),
            review_watch: Arc::new(crate::trading::autopilot::ReviewWatch::default()),
            weekend_watch: Arc::new(crate::trading::autopilot::ReviewWatch::default()),
            decision_health: Arc::new(crate::trading::autopilot::DecisionHealth::default()),
            rotation: Arc::new(AtomicUsize::new(0)),
            equity_guard: Arc::new(EquityGuard::new()),
            order_admission: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Attaches the configured market-data integration, if any.
    pub fn with_market(mut self, market: Option<MarketRuntime>) -> Self {
        self.market = market;
        self
    }

    /// Pins the wall clock so window-dependent behaviour is deterministic in
    /// tests; production never calls this and reads the real clock.
    pub fn with_fixed_now(mut self, now: Option<std::time::SystemTime>) -> Self {
        self.fixed_now = now;
        self
    }

    /// The current instant: the pinned test clock when set, else the wall clock.
    pub fn now(&self) -> std::time::SystemTime {
        self.fixed_now.unwrap_or_else(std::time::SystemTime::now)
    }

    /// Attaches the durable runtime-state facade.
    pub fn with_runtime_state(mut self, runtime_state: RuntimeState) -> Self {
        self.runtime_state = runtime_state;
        self
    }

    /// Attaches the configured calendar integration, if any.
    pub fn with_calendar(mut self, calendar: Option<CalendarRuntime>) -> Self {
        self.calendar = calendar;
        self
    }

    /// Attaches the configured autonomous loop settings, if any.
    pub fn with_autopilot(self, autopilot: Option<AutopilotSettings>) -> Self {
        *self.autopilot_slot() = autopilot;
        self
    }

    /// Attaches the writable overlay in front of the environment.
    pub fn with_runtime_config(mut self, config: crate::runtime_config::RuntimeConfig) -> Self {
        self.runtime_config = config;
        self
    }

    /// Attaches the optional encrypted console credential boundary.
    pub fn with_credential_vault(
        mut self,
        vault: Option<crate::credential::CredentialVault>,
    ) -> Self {
        self.credential_vault = vault;
        self
    }

    /// Replaces the model cooldown registry, e.g. with one on an injected
    /// clock. Attach it before building a model runtime: engines capture the
    /// registry they are built with.
    pub fn with_model_cooldowns(mut self, cooldowns: crate::model::CooldownRegistry) -> Self {
        self.model_cooldowns = cooldowns;
        self
    }

    /// Per-candidate model cooldowns shared by every engine the service
    /// builds, so they survive rebuilds.
    pub fn model_cooldowns(&self) -> &crate::model::CooldownRegistry {
        &self.model_cooldowns
    }

    /// Returns the subscription OAuth state shared by control routes and the provider runtime.
    pub fn subscription_auth(&self) -> &crate::subscription_auth::SubscriptionAuthState {
        &self.subscription_auth
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

    /// Returns the active calendar integration, if one is configured.
    pub fn calendar(&self) -> Option<&CalendarRuntime> {
        self.calendar.as_ref()
    }

    /// Durable runtime-state facade (disabled without a database).
    pub fn runtime_state(&self) -> &RuntimeState {
        &self.runtime_state
    }

    /// Returns a snapshot of the autonomous loop settings, if any.
    ///
    /// This clones rather than lending: the settings can change under a
    /// running tick, and a tick that read half its configuration from before
    /// an edit and half from after would be far harder to reason about than
    /// one that finishes on the settings it started with.
    pub fn autopilot(&self) -> Option<AutopilotSettings> {
        self.autopilot_slot().clone()
    }

    /// Replaces the live autopilot settings.
    pub fn set_autopilot(&self, settings: Option<AutopilotSettings>) {
        *self.autopilot_slot() = settings;
    }

    fn autopilot_slot(&self) -> std::sync::RwLockWriteGuard<'_, Option<AutopilotSettings>> {
        self.autopilot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The writable overlay in front of the environment.
    pub fn runtime_config(&self) -> &crate::runtime_config::RuntimeConfig {
        &self.runtime_config
    }

    /// Configured console credential vault, if secure persistence is enabled.
    pub fn credential_vault(&self) -> Option<&crate::credential::CredentialVault> {
        self.credential_vault.as_ref()
    }

    /// Whether the service half of the execution control is armed.
    ///
    /// Prefer this over [`ServiceConfig::trading_enabled`]: the startup value
    /// is only the baseline, and the console can move it.
    pub fn trading_enabled(&self) -> bool {
        self.trading_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Arms or disarms the service half of the execution control.
    pub fn set_trading_enabled(&self, enabled: bool) {
        self.trading_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Entry-risk memory shared by the autopilot's stop policies; survives
    /// across ticks for the lifetime of the process.
    pub fn stop_basis(&self) -> &Arc<StopBasis> {
        &self.stop_basis
    }

    /// Floating-profit high-water marks and re-entry cooldowns shared across
    /// deterministic harvest checks.
    pub fn profit_harvest_book(&self) -> &Arc<ProfitHarvestBook> {
        &self.profit_harvest_book
    }

    /// Market the entry sweep last judged each instrument on; survives across
    /// ticks for the lifetime of the process.
    pub fn entry_watch(&self) -> &Arc<crate::trading::autopilot::EntryWatch> {
        &self.entry_watch
    }

    /// Judge answers held against the candle that produced them.
    pub fn judgements(&self) -> &Arc<crate::trading::autopilot::JudgementCache> {
        &self.judgements
    }

    /// Candle each open position was last reviewed on.
    pub fn review_watch(&self) -> &Arc<crate::trading::autopilot::ReviewWatch> {
        &self.review_watch
    }

    /// Friday close each open position was last given a weekend verdict on;
    /// the same watch shape, keyed by the close instant rather than a candle.
    pub fn weekend_watch(&self) -> &Arc<crate::trading::autopilot::ReviewWatch> {
        &self.weekend_watch
    }

    /// Whether decisions are completing, so a surface can say when they stop.
    pub fn decision_health(&self) -> &Arc<crate::trading::autopilot::DecisionHealth> {
        &self.decision_health
    }

    /// Returns a snapshot of the active model integration, if one is
    /// configured. Cloned for the same reason as [`AppState::autopilot`].
    ///
    /// A runtime whose ChatGPT subscription route no longer matches the live
    /// connection — it connected, disconnected, or a refresh was rejected
    /// since the runtime was built — is rebuilt here first, so every caller
    /// sees a route in step with the connection. The subscription leg is also
    /// gated live, so a caller holding an older snapshot never reaches a
    /// disconnected subscription either.
    pub fn model(&self) -> Option<ModelRuntime> {
        let current = self
            .model
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match current {
            Some(runtime) if runtime.subscription_route_stale(&self.subscription_auth) => {
                self.rebuild_stale_model(runtime)
            }
            other => other,
        }
    }

    /// Rebuilds a runtime whose subscription route went stale, carrying the
    /// call-budget windows across. A failed rebuild keeps the old runtime,
    /// whose live gate already keeps it off a disconnected subscription.
    fn rebuild_stale_model(&self, stale: ModelRuntime) -> Option<ModelRuntime> {
        let Some(settings) = stale.settings().cloned() else {
            return Some(stale);
        };
        let rebuilt = match ModelRuntime::from_settings_with_app(settings, self) {
            Ok(rebuilt) => rebuilt,
            Err(error) => {
                tracing::warn!(%error, "model route could not be rebuilt after a subscription change");
                return Some(stale);
            }
        };
        if let Err(error) = rebuilt.restore_state(&stale.state_snapshot()) {
            tracing::warn!(%error, "model call budget could not be carried across a route rebuild");
        }
        let mut slot = self
            .model
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match slot.as_ref() {
            // Install only over the runtime that went stale; another caller
            // (or a settings edit) may have replaced it meanwhile.
            Some(current) if current.same_instance(&stale) => {
                tracing::info!(
                    preferred = rebuilt.prefers_subscription(),
                    "ChatGPT subscription connection changed; model route rebuilt"
                );
                *slot = Some(rebuilt.clone());
                Some(rebuilt)
            }
            // Replaced or disabled meanwhile: report what is installed now.
            other => other.cloned(),
        }
    }

    /// Replaces the live model integration.
    pub fn set_model(&self, model: Option<ModelRuntime>) {
        *self
            .model
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = model;
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

    /// Serializes the final account revalidation and open-order enqueue so two
    /// callers cannot both authorize against the same pre-trade snapshot.
    pub fn order_admission(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.order_admission
    }
}
