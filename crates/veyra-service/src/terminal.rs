//! What Veyra remembers about the trading terminal between snapshots and
//! across restarts: the broker-clock offset last measured from a fresh quote,
//! and the terminal build and EA version it last reported.
//!
//! * The offset lets [`crate::broker_clock`] keep the broker's own clock over
//!   a quiet market or a weekend, instead of falling back to the terminal
//!   host's clock, which is only right when the host runs on broker time.
//! * The build lets [`watch`] notice that the terminal updated itself (the
//!   vendor's auto-update cannot be switched off) and say so once, even when
//!   the update happened while Veyra was down.
//!
//! Both are restored from and snapshotted to the durable runtime state.
//! Nothing here contacts the venue: [`watch`] only reads the retained link
//! report, records an audit event and queues a notification.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use crate::AppState;

/// How often [`watch`] reads the terminal's reported build.
pub const WATCH_PERIOD: Duration = Duration::from_secs(30);

/// Two quote readings of a new offset must be at least this far apart
/// before the offset is trusted.
pub const CONFIRM_SECS: i64 = 60;

/// How far two readings' drifts may differ and still agree. A frozen quote
/// drifts by the whole time between readings, so it can never agree.
pub const AGREE_SECS: i64 = 30;

/// A quote reading of an offset not yet confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    offset_secs: i64,
    drift_secs: i64,
    received_utc_secs: i64,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Memory {
    offset_secs: Option<i64>,
    candidate: Option<Candidate>,
    build: Option<u32>,
    ea_version: Option<String>,
}

/// A terminal build change: the vendor updated the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildChange {
    /// Build reported before.
    pub from: u32,
    /// Build reported now.
    pub to: u32,
    /// EA version reported with the new build, when known.
    pub ea_version: Option<String>,
}

/// Process-wide terminal memory; see the module notes.
#[derive(Debug, Default)]
pub struct TerminalMemory {
    inner: Mutex<Memory>,
}

impl TerminalMemory {
    /// An empty memory.
    pub fn new() -> Self {
        Self::default()
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Last broker-clock offset measured from a fresh quote, in seconds.
    pub fn offset_secs(&self) -> Option<i64> {
        self.memory().offset_secs
    }

    /// Whether a quote reading (`drift_secs` = quote time − receipt time,
    /// rounding to `offset_secs`) can be trusted, remembering it when it can.
    ///
    /// The known offset is trusted at once. A different one, which a stale
    /// quote can imitate by landing near a quarter hour, is trusted only
    /// after readings at least [`CONFIRM_SECS`] apart agree within
    /// [`AGREE_SECS`]: a live quote keeps its drift, a frozen one does not.
    pub fn confirm_quote(&self, offset_secs: i64, drift_secs: i64, received_utc_secs: i64) -> bool {
        let mut memory = self.memory();
        if memory.offset_secs == Some(offset_secs) {
            memory.candidate = None;
            return true;
        }
        match memory.candidate {
            Some(candidate)
                if candidate.offset_secs == offset_secs
                    && (drift_secs - candidate.drift_secs).abs() <= AGREE_SECS =>
            {
                if received_utc_secs - candidate.received_utc_secs >= CONFIRM_SECS {
                    memory.offset_secs = Some(offset_secs);
                    memory.candidate = None;
                    true
                } else {
                    false
                }
            }
            _ => {
                memory.candidate = Some(Candidate {
                    offset_secs,
                    drift_secs,
                    received_utc_secs,
                });
                false
            }
        }
    }

    /// Terminal build and EA version last reported.
    pub fn terminal(&self) -> (Option<u32>, Option<String>) {
        let memory = self.memory();
        (memory.build, memory.ea_version.clone())
    }

    /// Records the reported build and EA version; returns the change when a
    /// previously known build differs. The first build ever seen is a
    /// baseline, not a change.
    pub fn observe_build(&self, build: u32, ea_version: Option<&str>) -> Option<BuildChange> {
        let mut memory = self.memory();
        let previous = memory.build.replace(build);
        if let Some(version) = ea_version {
            memory.ea_version = Some(version.to_owned());
        }
        match previous {
            Some(from) if from != build => Some(BuildChange {
                from,
                to: build,
                ea_version: memory.ea_version.clone(),
            }),
            _ => None,
        }
    }

    /// Serializable snapshot for the runtime state.
    pub fn state_snapshot(&self) -> Value {
        let memory = self.memory();
        json!({
            "offsetSecs": memory.offset_secs,
            "build": memory.build,
            "eaVersion": memory.ea_version,
        })
    }

    /// Restores a stored snapshot; absent fields stay unknown.
    ///
    /// # Errors
    /// Returns a description when a present value is unusable.
    pub fn restore_state(&self, value: &Value) -> Result<(), String> {
        let offset_secs = match value.get("offsetSecs") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(
                raw.as_i64()
                    .filter(|secs| secs.abs() <= 14 * 3_600)
                    .ok_or_else(|| "terminal `offsetSecs` must be within ±14 h".to_owned())?,
            ),
        };
        let build = match value.get("build") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(
                raw.as_u64()
                    .and_then(|build| u32::try_from(build).ok())
                    .ok_or_else(|| "terminal `build` must be a positive integer".to_owned())?,
            ),
        };
        let ea_version = match value.get("eaVersion") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(
                raw.as_str()
                    .filter(|version| version.len() <= 16)
                    .ok_or_else(|| "terminal `eaVersion` must be a short string".to_owned())?
                    .to_owned(),
            ),
        };
        *self.memory() = Memory {
            offset_secs,
            candidate: None,
            build,
            ea_version,
        };
        Ok(())
    }
}

/// The notification for a build change.
pub fn build_change_notification(change: &BuildChange) -> crate::notify::Notification {
    crate::notify::Notification::new(
        crate::notify::NotifyEvent::BrokerLink,
        crate::notify::Severity::Warning,
        format!("Terminal updated to build {}", change.to),
        format!(
            "MetaTrader updated itself from build {} to {}. Veyra keeps trading; watch the next orders and stop changes for new rejections.",
            change.from, change.to
        ),
    )
}

/// Reads the terminal's reported build every [`WATCH_PERIOD`] for the life
/// of the process; see [`check`].
pub async fn watch(state: AppState) {
    if state.broker().is_none() {
        return;
    }
    let mut cadence = actix_web::rt::time::interval(WATCH_PERIOD);
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        cadence.tick().await;
        check(&state).await;
    }
}

/// Compares the terminal's freshly reported build with the remembered one;
/// on a change, records an audit event, queues a notification, saves the
/// memory and returns the change. Reads state only.
pub async fn check(state: &AppState) -> Option<BuildChange> {
    let broker = state.broker()?;
    let report = broker.link().report().await;
    let snapshot = report.snapshot.filter(|_| report.fresh)?;
    let build = snapshot.terminal_build()?;
    let change = state
        .terminal_memory()
        .observe_build(build, snapshot.ea_version())?;
    tracing::warn!(from = change.from, to = change.to, "terminal build changed");
    if let Some(audit) = state.audit() {
        audit
            .try_record(crate::audit::AuditEvent::new(
                crate::audit::AuditKind::TerminalChanged,
                json!({
                    "fromBuild": change.from,
                    "toBuild": change.to,
                    "eaVersion": change.ea_version,
                }),
            ))
            .await;
    }
    if state
        .notifier()
        .wants(crate::notify::NotifyEvent::BrokerLink)
    {
        state.notifier().notify(build_change_notification(&change));
    }
    let runtime = state.runtime_state();
    if runtime.enabled() {
        runtime
            .save(
                crate::state::StateKey::Terminal,
                &state.terminal_memory().state_snapshot(),
            )
            .await;
    }
    Some(change)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_terminal() -> (
        AppState,
        std::sync::Arc<crate::broker::EaLink>,
        std::sync::Arc<crate::audit::MemoryTrail>,
    ) {
        let settings = crate::broker::BrokerSettings::from_source(|name| match name {
            "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
            "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
            _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings")
        .expect("configured");
        let runtime = crate::broker::BrokerRuntime::from_settings(settings).expect("runtime");
        let link = runtime.ea_link().expect("ea link");
        let config = crate::config::ServiceConfig::from_source(|name| match name {
            "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
            "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
            "VEYRA_ENV" => Ok("development".to_owned()),
            _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("config");
        let trail = std::sync::Arc::new(crate::audit::MemoryTrail::default());
        let state = AppState::new(
            config,
            Some(runtime),
            None,
            crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
        )
        .with_audit(Some(crate::audit::AuditRuntime::new(trail.clone())));
        (state, link, trail)
    }

    fn snapshot(build: u32) -> crate::broker::AccountSnapshot {
        crate::broker::AccountSnapshot::new(
            crate::broker::AccountLogin::parse(94_168).expect("login"),
            crate::broker::ServerName::parse("IFCMarkets-Real").expect("server"),
            crate::broker::Symbol::parse("EURUSD").expect("symbol"),
            true,
            true,
            0,
            0.0,
        )
        .with_terminal(Some(build), Some("1.27".to_owned()))
    }

    #[actix_web::test]
    async fn a_terminal_update_is_recorded_once() {
        let (state, link, trail) = state_with_terminal();
        assert_eq!(check(&state).await, None, "nothing reported yet");
        link.record(snapshot(1_440));
        assert_eq!(check(&state).await, None, "the first build is a baseline");
        link.record(snapshot(1_445));
        let change = check(&state).await.expect("change");
        assert_eq!((change.from, change.to), (1_440, 1_445));
        assert_eq!(check(&state).await, None, "reported once");
        let recorded: Vec<_> = trail
            .events()
            .into_iter()
            .filter(|event| event.kind() == crate::audit::AuditKind::TerminalChanged)
            .collect();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].payload()["toBuild"], 1_445);
        // Without a broker there is nothing to watch.
        let (bare, _, _) = state_with_terminal();
        let bare = AppState::new(
            bare.config().clone(),
            None,
            None,
            crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
        );
        assert_eq!(check(&bare).await, None);
        watch(bare).await;
    }

    #[test]
    fn the_first_build_is_a_baseline_and_each_change_is_reported_once() {
        let memory = TerminalMemory::new();
        assert_eq!(memory.observe_build(1440, Some("1.27")), None);
        assert_eq!(memory.observe_build(1440, Some("1.27")), None);
        let change = memory.observe_build(1445, None).expect("change");
        assert_eq!(
            change,
            BuildChange {
                from: 1440,
                to: 1445,
                ea_version: Some("1.27".to_owned()),
            }
        );
        assert_eq!(memory.observe_build(1445, None), None);
        assert_eq!(memory.terminal(), (Some(1445), Some("1.27".to_owned())));
        let notification = build_change_notification(&change);
        assert_eq!(notification.title, "Terminal updated to build 1445");
        assert!(notification.body.contains("from build 1440 to 1445"));
    }

    #[test]
    fn a_new_offset_needs_agreeing_readings_and_a_frozen_quote_never_agrees() {
        let memory = TerminalMemory::new();
        // A live quote: the same drift a minute apart.
        assert!(!memory.confirm_quote(7_200, 7_180, 1_000));
        assert!(!memory.confirm_quote(7_200, 7_185, 1_030), "too soon");
        assert!(memory.confirm_quote(7_200, 7_175, 1_060));
        assert_eq!(memory.offset_secs(), Some(7_200));
        // The known offset is trusted at once.
        assert!(memory.confirm_quote(7_200, 7_150, 1_100));
        // A frozen quote that happens to round to 5,400: its drift shrinks by
        // the time between readings, so it never confirms.
        assert!(!memory.confirm_quote(5_400, 5_200, 2_000));
        assert!(!memory.confirm_quote(5_400, 5_140, 2_060));
        assert!(!memory.confirm_quote(5_400, 5_080, 2_120));
        assert_eq!(memory.offset_secs(), Some(7_200));
        // A real change (daylight saving) confirms after a minute.
        assert!(!memory.confirm_quote(10_800, 10_790, 3_000));
        assert!(memory.confirm_quote(10_800, 10_795, 3_070));
        assert_eq!(memory.offset_secs(), Some(10_800));
    }

    #[test]
    fn memory_survives_a_restart_and_refuses_unusable_values() {
        let memory = TerminalMemory::new();
        memory.confirm_quote(7_200, 7_200, 0);
        assert!(memory.confirm_quote(7_200, 7_200, CONFIRM_SECS));
        memory.observe_build(1440, Some("1.27"));
        let restored = TerminalMemory::new();
        restored
            .restore_state(&memory.state_snapshot())
            .expect("restore");
        assert_eq!(restored.offset_secs(), Some(7_200));
        // A change while Veyra was down is still a change.
        assert!(restored.observe_build(1441, None).is_some());

        let empty = TerminalMemory::new();
        empty.restore_state(&json!({})).expect("empty");
        assert_eq!(empty.offset_secs(), None);
        assert!(
            empty
                .restore_state(&json!({ "offsetSecs": 15 * 3_600 }))
                .is_err()
        );
        assert!(empty.restore_state(&json!({ "build": -1 })).is_err());
        assert!(empty.restore_state(&json!({ "eaVersion": 7 })).is_err());
    }
}
