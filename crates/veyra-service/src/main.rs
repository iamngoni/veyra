//! Minimal process wiring: fail before listening if configuration, logging, or
//! broker settings are invalid, then serve the diagnostic surface plus any
//! provider-required loopback listener. This binary has no execution path.

use std::sync::Arc;
use std::time::Duration;

use veyra_service::audit::{AuditEvent, AuditKind, AuditRuntime, AuditTrail};
use veyra_service::broker::{BrokerRuntime, BrokerSettings};
use veyra_service::calendar::{CalendarRuntime, CalendarSettings};
use veyra_service::jev::{JevRuntime, JevSettings};
use veyra_service::logs::{self, LogBuffer};
use veyra_service::market::{MarketRuntime, MarketSettings};
use veyra_service::model::{ModelRuntime, settings::ModelSettings};
use veyra_service::reconciliation;
use veyra_service::risk::{RiskGate, RiskPolicy, RiskPolicyPatch};
use veyra_service::state::{RuntimeState, StateKey};
use veyra_service::store::Store;
use veyra_service::trading::autopilot::{AutopilotSettings, TickOutcome};
use veyra_service::{AppState, config::ServiceConfig, observability, server};

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServiceConfig::from_env()?;
    let logs = LogBuffer::new(logs::DEFAULT_CAPACITY);
    observability::init(logs.clone())?;

    // The database opens before anything reads runtime state: durable
    // counters, baselines, and the operator's live risk policy are restored
    // from it, and a configured database that is unreachable fails startup
    // rather than silently running without an audit trail.
    let (runtime_state, audit) = match config.database_url() {
        Some(url) => {
            let store = Arc::new(Store::connect(url).await?);
            store.migrate().await?;
            let trail: Arc<dyn AuditTrail> = store.clone();
            let state_store: Arc<dyn veyra_service::state::StateStore> = store.clone();
            (
                RuntimeState::new(Some(state_store)),
                Some(Arc::new(AuditRuntime::new(trail))),
            )
        }
        None => (RuntimeState::disabled(), None),
    };

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

    // The economic calendar is optional: absent configuration leaves the
    // blackout inert and the model without news context.
    let calendar = match CalendarSettings::from_env()? {
        Some(settings) => Some(CalendarRuntime::from_settings(settings)?),
        None => None,
    };

    let autopilot = AutopilotSettings::from_env()?;

    let companion = match &broker {
        Some(runtime) => runtime.listener()?,
        None => None,
    };

    // The gate is always present and restrictive by default: an unconfigured
    // allowlist approves nothing, so a missing setting cannot widen behavior.
    // A stored policy snapshot (console edits) is applied over the environment
    // baseline, validated by exactly the same rules.
    let mut policy = RiskPolicy::from_env()?;
    if let Some(stored) = runtime_state.load(StateKey::RiskPolicy).await {
        let patch: RiskPolicyPatch = serde_json::from_value(stored)
            .map_err(|error| format!("stored risk policy is unreadable: {error}"))?;
        policy = policy
            .apply_patch(&patch)
            .map_err(|error| format!("stored risk policy is invalid: {error}"))?;
        tracing::info!("restored the live risk policy from durable state");
    }
    let risk = RiskGate::new(policy);

    let listener = server::bind(&config)?;
    let state = AppState::new(config, broker, model, risk)
        .with_market(market)
        .with_calendar(calendar)
        .with_autopilot(autopilot)
        .with_jev(jev)
        .with_logs(logs)
        .with_runtime_state(runtime_state.clone())
        .with_audit(audit.as_ref().map(|runtime| (**runtime).clone()));

    // Counters and baselines resume before the first tick can move them.
    restore_runtime_state(&state, &runtime_state).await;

    // A panic in a spawned task kills that task quietly: the autopilot can stop
    // deciding while the process still answers /health. The hook records the
    // panic durably first, so the failure outlives both the task and the
    // in-memory log ring a restart would clear.
    if let Some(runtime) = audit.clone() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = serde_json::json!({
                "outcome": "panic",
                "origin": "process",
                "detail": info.to_string(),
                "location": info.location().map(|at| at.to_string()),
                "thread": std::thread::current().name().unwrap_or("unnamed"),
            });
            let runtime = runtime.clone();
            // The hook is synchronous and runs on the panicking thread, which
            // may have no reactor of its own, so the write gets a dedicated
            // one rather than assuming the caller's is usable.
            std::thread::spawn(move || {
                actix_web::rt::System::new().block_on(async {
                    runtime
                        .try_record(AuditEvent::new(AuditKind::Failure, payload))
                        .await;
                });
            })
            .join()
            .ok();
            previous(info);
        }));
    }

    if let Some(runtime) = &audit {
        if let Some(broker) = state.broker() {
            broker.link().attach_audit(runtime.clone());
        }
        runtime
            .try_record(AuditEvent::new(
                AuditKind::ServiceStarted,
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    // The effective risk rules travel with the trail, so any
                    // decision can be read against the policy in force.
                    "riskPolicy": state.risk().policy().summary()
                }),
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

    // Durable counters and baselines are snapshotted on a cadence: a crash
    // loses at most one interval, a restart resumes where it left off. The
    // risk policy is saved immediately on every accepted console edit.
    if runtime_state.enabled() {
        let snapshot_state = state.clone();
        let snapshot_runtime = runtime_state.clone();
        actix_web::rt::spawn(async move {
            let period = Duration::from_secs(60);
            loop {
                actix_web::rt::time::sleep(period).await;
                persist_runtime_state(&snapshot_state, &snapshot_runtime).await;
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
                    // The quiet majority once the entry gate is doing its job:
                    // logging it at info would bury the ticks that decided
                    // something under one line a minute saying nothing did.
                    TickOutcome::Unchanged => {
                        tracing::debug!("autopilot tick found nothing changed");
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

/// Restores durable counters and baselines. Unusable values log and fall back
/// to in-memory defaults rather than blocking startup.
async fn restore_runtime_state(state: &AppState, runtime: &RuntimeState) {
    if let Some(value) = runtime.load(StateKey::JevUsage).await
        && let Some(jev) = state.jev()
        && let Err(error) = jev.restore_state(&value)
    {
        tracing::warn!(%error, "stored judge usage is unusable; starting from zero");
    }
    if let Some(value) = runtime.load(StateKey::ModelBudget).await
        && let Some(model) = state.model()
        && let Err(error) = model.restore_state(&value)
    {
        tracing::warn!(%error, "stored model budget is unusable; starting fresh");
    }
    if let Some(value) = runtime.load(StateKey::EquityBaselines).await
        && let Err(error) = state.equity_guard().restore_state(&value)
    {
        tracing::warn!(%error, "stored equity baselines are unusable; re-baselining");
    }
    if let Some(value) = runtime.load(StateKey::StopBasis).await
        && let Err(error) = state.stop_basis().restore_state(&value)
    {
        tracing::warn!(%error, "stored stop basis is unusable; re-learning");
    }
}

/// Snapshots durable counters and baselines; best-effort by design.
async fn persist_runtime_state(state: &AppState, runtime: &RuntimeState) {
    if !runtime.enabled() {
        return;
    }
    if let Some(jev) = state.jev() {
        runtime
            .save(StateKey::JevUsage, &jev.state_snapshot())
            .await;
    }
    if let Some(model) = state.model() {
        runtime
            .save(StateKey::ModelBudget, &model.state_snapshot())
            .await;
    }
    runtime
        .save(
            StateKey::EquityBaselines,
            &state.equity_guard().state_snapshot(),
        )
        .await;
    runtime
        .save(StateKey::StopBasis, &state.stop_basis().state_snapshot())
        .await;
}
