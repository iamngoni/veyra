//! External watchdog: `veyra-service watchdog`.
//!
//! The in-process notifier cannot report that its own process died, so this
//! mode runs as a separate process (its own container) and probes the
//! service's `/ready` endpoint. It reports, through the same providers the
//! console configures, when the service stops answering or its database is
//! unavailable, and again when it recovers.
//!
//! Boundaries: it never runs migrations and never writes state. It only reads
//! the saved notification settings (opening the sealed secrets with the same
//! credential vault) and re-reads them periodically so console edits apply.
//! A database that is down at startup is retried rather than fatal, because
//! the watchdog must keep running to report exactly that kind of outage.

use std::time::Duration;

use serde_json::Value;

use super::{Notification, Notifier, NotifyEvent, Severity};
use crate::config::ConfigError;
use crate::credential::CredentialVault;
use crate::state::RuntimeState;
use crate::store::Store;

const URL_ENV: &str = "VEYRA_WATCHDOG_URL";
const INTERVAL_ENV: &str = "VEYRA_WATCHDOG_INTERVAL_SECS";
const FAILURES_ENV: &str = "VEYRA_WATCHDOG_FAILURES";

/// Service address probed when none is configured (the compose service name).
pub const DEFAULT_URL: &str = "http://veyra:8080";

/// How often notification settings are re-read.
const SETTINGS_REFRESH: Duration = Duration::from_secs(300);

/// Validated watchdog settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogSettings {
    ready_url: String,
    interval: Duration,
    failures: u32,
}

fn optional(
    source: &mut impl FnMut(&'static str) -> Result<String, ConfigError>,
    name: &'static str,
) -> String {
    source(name)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

fn bounded(
    raw: &str,
    name: &'static str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ConfigError> {
    if raw.is_empty() {
        return Ok(default);
    }
    raw.parse::<u64>()
        .ok()
        .filter(|value| (min..=max).contains(value))
        .ok_or(ConfigError::InvalidEnvironmentVariable {
            name,
            reason: "must be a whole number inside the documented range",
        })
}

impl WatchdogSettings {
    /// Reads `VEYRA_WATCHDOG_URL` (service base URL, default
    /// [`DEFAULT_URL`]), `VEYRA_WATCHDOG_INTERVAL_SECS` (10-3600, default 60)
    /// and `VEYRA_WATCHDOG_FAILURES` (failed probes before alerting, 1-60,
    /// default 3).
    ///
    /// # Errors
    /// Returns [`ConfigError`] for a malformed URL or an out-of-range number.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let raw_url = optional(&mut source, URL_ENV);
        let base = if raw_url.is_empty() {
            DEFAULT_URL.to_owned()
        } else {
            raw_url
        };
        let parsed = reqwest::Url::parse(&base)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
            .ok_or(ConfigError::InvalidEnvironmentVariable {
                name: URL_ENV,
                reason: "must be an http(s) URL such as http://veyra:8080",
            })?;
        let ready_url = format!("{}/ready", parsed.as_str().trim_end_matches('/'));
        let interval = bounded(
            &optional(&mut source, INTERVAL_ENV),
            INTERVAL_ENV,
            60,
            10,
            3_600,
        )?;
        let failures = bounded(&optional(&mut source, FAILURES_ENV), FAILURES_ENV, 3, 1, 60)?;
        Ok(Self {
            ready_url,
            interval: Duration::from_secs(interval),
            failures: u32::try_from(failures).unwrap_or(3),
        })
    }

    /// The readiness URL probed.
    pub fn ready_url(&self) -> &str {
        &self.ready_url
    }
}

/// What one probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The service answered and its database is reachable.
    Ready,
    /// The service answered but its audit database is not.
    DatabaseUnavailable,
    /// No usable answer; the reason is secret-free.
    Unreachable(String),
}

/// Classifies a `/ready` answer. A stale broker is not the watchdog's
/// concern: the running service reports that itself.
pub fn classify(status: u16, body: &Value) -> Probe {
    if !(200..300).contains(&status) {
        return Probe::Unreachable(format!("/ready answered HTTP {status}"));
    }
    match body.get("audit").and_then(Value::as_str) {
        Some("unavailable" | "timeout") => Probe::DatabaseUnavailable,
        _ => Probe::Ready,
    }
}

/// Reports an outage after enough consecutive failed probes, and its end.
#[derive(Debug, Default)]
pub struct DownTracker {
    failures: u32,
    alerted: bool,
}

impl DownTracker {
    /// The notification this probe warrants, if any.
    pub fn observe(&mut self, probe: &Probe, threshold: u32) -> Option<Notification> {
        match probe {
            Probe::Ready => {
                self.failures = 0;
                if !self.alerted {
                    return None;
                }
                self.alerted = false;
                Some(Notification::new(
                    NotifyEvent::ServiceDown,
                    Severity::Info,
                    "Veyra is back",
                    "The service is answering and its database is reachable.",
                ))
            }
            failure => {
                self.failures = self.failures.saturating_add(1);
                if self.alerted || self.failures < threshold {
                    return None;
                }
                self.alerted = true;
                let (title, body) = match failure {
                    Probe::DatabaseUnavailable => (
                        "Veyra's database is unavailable",
                        "The service is running but cannot read or write its journal.".to_owned(),
                    ),
                    Probe::Unreachable(reason) => (
                        "Veyra is not responding",
                        format!(
                            "{reason}. Trading and position management are stopped until it is back."
                        ),
                    ),
                    Probe::Ready => ("", String::new()),
                };
                Some(Notification::new(
                    NotifyEvent::ServiceDown,
                    Severity::Critical,
                    title,
                    body,
                ))
            }
        }
    }
}

pub(crate) async fn probe(client: &reqwest::Client, url: &str) -> Probe {
    match client.get(url).send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.json::<Value>().await.unwrap_or(Value::Null);
            classify(status, &body)
        }
        Err(error) => Probe::Unreachable(error.without_url().to_string()),
    }
}

async fn open_state(url: &str) -> RuntimeState {
    let mut wait = Duration::from_secs(5);
    loop {
        match Store::connect(url).await {
            Ok(store) => {
                let store: std::sync::Arc<dyn crate::state::StateStore> =
                    std::sync::Arc::new(store);
                return RuntimeState::new(Some(store));
            }
            Err(error) => {
                tracing::warn!(%error, "watchdog cannot reach the database yet; retrying");
                actix_web::rt::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// Runs the watchdog until the process is stopped.
///
/// # Errors
/// Returns an error for invalid settings, a missing database URL or vault,
/// or an HTTP client that cannot be built. Outages it observes are never
/// errors; they are what it reports.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let settings = WatchdogSettings::from_source(|name| {
        std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
    })?;
    let database = std::env::var("VEYRA_DATABASE_URL")
        .map_err(|_| "the watchdog needs VEYRA_DATABASE_URL to read notification settings")?;
    let vault = CredentialVault::from_env()?
        .ok_or("the watchdog needs the console secret key to read notification credentials")?;
    let (notifier, worker) = Notifier::new(true)?;
    actix_web::rt::spawn(worker.run());
    let state = open_state(&database).await;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()?;
    tracing::info!(url = settings.ready_url(), "watchdog started");

    let mut tracker = DownTracker::default();
    let mut refreshed: Option<std::time::Instant> = None;
    let mut cadence = actix_web::rt::time::interval(settings.interval);
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        cadence.tick().await;
        if refreshed.is_none_or(|at| at.elapsed() >= SETTINGS_REFRESH) {
            // Keep the last good settings through a database outage.
            match super::routes::load(&state, &vault).await {
                Ok(config) => {
                    notifier.set_config(config);
                    refreshed = Some(std::time::Instant::now());
                }
                Err(reason) => {
                    tracing::warn!(%reason, "watchdog kept its last notification settings")
                }
            }
        }
        let result = probe(&client, settings.ready_url()).await;
        if let Some(notification) = tracker.observe(&result, settings.failures) {
            notifier.notify(notification);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn settings(values: &[(&'static str, &'static str)]) -> Result<WatchdogSettings, ConfigError> {
        WatchdogSettings::from_source(|name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(ConfigError::MissingEnvironmentVariable { name })
        })
    }

    #[test]
    fn settings_default_to_the_compose_service_and_validate_ranges() {
        let defaults = settings(&[]).expect("defaults");
        assert_eq!(defaults.ready_url(), "http://veyra:8080/ready");
        assert_eq!(defaults.interval, Duration::from_secs(60));
        assert_eq!(defaults.failures, 3);

        let custom = settings(&[
            (URL_ENV, "https://veyra.example/"),
            (INTERVAL_ENV, "30"),
            (FAILURES_ENV, "5"),
        ])
        .expect("custom");
        assert_eq!(custom.ready_url(), "https://veyra.example/ready");
        assert_eq!(custom.interval, Duration::from_secs(30));

        assert!(settings(&[(URL_ENV, "veyra:8080")]).is_err());
        assert!(settings(&[(INTERVAL_ENV, "5")]).is_err());
        assert!(settings(&[(FAILURES_ENV, "0")]).is_err());
        assert!(settings(&[(FAILURES_ENV, "many")]).is_err());
    }

    #[test]
    fn readiness_answers_are_classified() {
        assert_eq!(
            classify(200, &json!({"status": "ready", "audit": "ok"})),
            Probe::Ready
        );
        assert_eq!(
            classify(
                200,
                &json!({"status": "degraded", "broker": "stale", "audit": "ok"})
            ),
            Probe::Ready,
            "a stale broker is reported by the service itself"
        );
        assert_eq!(
            classify(200, &json!({"audit": "timeout"})),
            Probe::DatabaseUnavailable
        );
        assert_eq!(
            classify(502, &Value::Null),
            Probe::Unreachable("/ready answered HTTP 502".to_owned())
        );
    }

    #[test]
    fn an_outage_is_reported_once_after_the_threshold_and_then_its_end() {
        let mut tracker = DownTracker::default();
        let down = Probe::Unreachable("connection refused".to_owned());
        assert!(tracker.observe(&Probe::Ready, 3).is_none());
        assert!(tracker.observe(&down, 3).is_none());
        assert!(tracker.observe(&down, 3).is_none());
        let alert = tracker.observe(&down, 3).expect("alert");
        assert_eq!(alert.event, Some(NotifyEvent::ServiceDown));
        assert_eq!(alert.title, "Veyra is not responding");
        assert!(alert.body.starts_with("connection refused."));
        assert!(tracker.observe(&down, 3).is_none(), "once per outage");
        let back = tracker.observe(&Probe::Ready, 3).expect("recovery");
        assert_eq!(back.title, "Veyra is back");
        assert!(tracker.observe(&Probe::Ready, 3).is_none());

        let database = tracker
            .observe(&Probe::DatabaseUnavailable, 1)
            .expect("database alert");
        assert_eq!(database.title, "Veyra's database is unavailable");
    }
}
