//! Operator notifications: fan-out of service events to every configured
//! provider (email, Telegram, Discord, Slack, ntfy, Pushover, webhook).
//!
//! Lifecycle and safety boundaries:
//!
//! * **Never blocks trading.** [`Notifier::notify`] only filters and
//!   `try_send`s onto a bounded queue. A full or closed queue drops the
//!   notification and counts it; no caller ever awaits delivery.
//! * **One worker, bounded fan-out.** [`NotifyWorker::run`] drains the queue.
//!   Each notification fans out to every enabled provider concurrently, each
//!   delivery with the shared client's timeouts and a small retry budget for
//!   transient failures. At most [`MAX_IN_FLIGHT`] notifications are in
//!   flight, so a slow provider back-pressures the queue instead of spawning
//!   without bound.
//! * **Secrets stay sealed.** Provider credentials are only ever held here and
//!   in the encrypted credential vault. [`Notifier::view`] reports whether each
//!   secret is set plus a four-character hint, never the value, and delivery
//!   errors are reported without request URLs (which carry tokens for
//!   Telegram and webhook providers).
//! * **Informational only.** Nothing here reads replies or accepts commands;
//!   a provider can never influence trading.

pub mod events;
pub mod providers;
pub mod routes;
pub mod watchdog;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::{Semaphore, mpsc};

pub use providers::ProviderKind;

/// Notifications waiting for the worker before new ones are dropped.
pub const QUEUE_CAPACITY: usize = 256;

/// Notifications being delivered at once.
pub const MAX_IN_FLIGHT: usize = 8;

/// Delivery attempts per provider for a transient failure.
const MAX_ATTEMPTS: u32 = 3;

/// First retry delay; each later retry doubles it (2 s, then 4 s).
#[cfg(not(test))]
const RETRY_BASE: Duration = Duration::from_secs(1);
#[cfg(test)]
const RETRY_BASE: Duration = Duration::from_millis(5);

/// Recent delivery outcomes kept for the console.
const RECENT_DELIVERIES: usize = 30;

/// Longest title a notification carries.
const MAX_TITLE_CHARS: usize = 120;

/// Longest body a notification carries (Discord's limit is 2000 in total).
const MAX_BODY_CHARS: usize = 1_500;

/// A category of notification the operator can switch on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyEvent {
    /// A daily-loss or peak-drawdown breaker started blocking new entries.
    BreakerTripped,
    /// Trading was halted: kill switch engaged or execution switched off.
    TradingHalted,
    /// The broker link went stale, or recovered after going stale.
    BrokerLink,
    /// Venue positions diverged from what Veyra manages.
    ReconciliationDrift,
    /// The terminal rejected or failed an order command.
    OrderFailed,
    /// The autopilot could not get a decision several times in a row.
    ModelTrouble,
    /// A position was opened.
    TradeOpened,
    /// A position was closed.
    TradeClosed,
    /// Once-a-day account summary.
    DailySummary,
    /// The external watchdog could not reach a ready service.
    ServiceDown,
}

impl NotifyEvent {
    /// Every event, in display order.
    pub const ALL: [Self; 10] = [
        Self::BreakerTripped,
        Self::TradingHalted,
        Self::BrokerLink,
        Self::ReconciliationDrift,
        Self::OrderFailed,
        Self::ModelTrouble,
        Self::ServiceDown,
        Self::TradeOpened,
        Self::TradeClosed,
        Self::DailySummary,
    ];

    /// Stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BreakerTripped => "breaker_tripped",
            Self::TradingHalted => "trading_halted",
            Self::BrokerLink => "broker_link",
            Self::ReconciliationDrift => "reconciliation_drift",
            Self::OrderFailed => "order_failed",
            Self::ModelTrouble => "model_trouble",
            Self::TradeOpened => "trade_opened",
            Self::TradeClosed => "trade_closed",
            Self::DailySummary => "daily_summary",
            Self::ServiceDown => "service_down",
        }
    }

    /// Parses a wire name.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_str() == raw)
    }
}

/// How urgent a notification is. Providers with priorities (ntfy, Pushover)
/// map it onto theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Routine information, e.g. a trade closed.
    Info,
    /// Something needs a look soon.
    Warning,
    /// Trading is affected now.
    Critical,
}

impl Severity {
    /// Short marker prefixed to chat-style messages.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Info => "ℹ️",
            Self::Warning => "⚠️",
            Self::Critical => "🚨",
        }
    }
}

/// One message for the operator. Construct with [`Notification::new`], which
/// clips the text to what every provider accepts.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// Category, or `None` for an operator-requested test message.
    pub event: Option<NotifyEvent>,
    /// Urgency.
    pub severity: Severity,
    /// One-line summary.
    pub title: String,
    /// Plain-text detail; may span lines.
    pub body: String,
    /// When the underlying event happened.
    pub at: OffsetDateTime,
}

impl Notification {
    /// Builds a notification stamped now, clipping title and body.
    pub fn new(
        event: NotifyEvent,
        severity: Severity,
        title: impl AsRef<str>,
        body: impl AsRef<str>,
    ) -> Self {
        Self::build(Some(event), severity, title.as_ref(), body.as_ref())
    }

    /// The message a "Send test" button delivers.
    pub fn test() -> Self {
        Self::build(
            None,
            Severity::Info,
            "Veyra test notification",
            "Notifications from Veyra will arrive here.",
        )
    }

    fn build(event: Option<NotifyEvent>, severity: Severity, title: &str, body: &str) -> Self {
        Self {
            event,
            severity,
            title: crate::text::clip(title.trim(), MAX_TITLE_CHARS),
            body: crate::text::clip(body.trim(), MAX_BODY_CHARS),
            at: OffsetDateTime::now_utc(),
        }
    }

    /// Stable wire name of the event, `"test"` for a test message.
    pub fn event_name(&self) -> &'static str {
        self.event.map_or("test", NotifyEvent::as_str)
    }
}

/// One provider's saved, non-secret settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPrefs {
    /// Whether notifications go to this provider.
    #[serde(default)]
    pub enabled: bool,
    /// Non-secret fields by name, e.g. `chatId` or `host`.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// Saved, non-secret notification settings. Stored in plain runtime state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyPrefs {
    /// Per-event switches; an absent event is on.
    #[serde(default)]
    pub events: BTreeMap<NotifyEvent, bool>,
    /// Per-provider settings; an absent provider is off.
    #[serde(default)]
    pub providers: BTreeMap<ProviderKind, ProviderPrefs>,
    /// UTC hour (0-23) the daily summary goes out.
    #[serde(default = "default_summary_hour")]
    pub summary_hour_utc: u8,
}

fn default_summary_hour() -> u8 {
    // 20:00 in Johannesburg is 18:00 UTC; any hour is fine as a default as
    // long as it falls after the main FX sessions for most operators.
    18
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        Self {
            events: BTreeMap::new(),
            providers: BTreeMap::new(),
            summary_hour_utc: default_summary_hour(),
        }
    }
}

impl NotifyPrefs {
    /// Whether `event` is switched on.
    pub fn event_enabled(&self, event: NotifyEvent) -> bool {
        self.events.get(&event).copied().unwrap_or(true)
    }

    /// Whether `kind` is switched on.
    pub fn provider_enabled(&self, kind: ProviderKind) -> bool {
        self.providers.get(&kind).is_some_and(|prefs| prefs.enabled)
    }
}

/// Provider secrets by provider, then field name. Only ever persisted sealed.
pub type NotifySecrets = BTreeMap<ProviderKind, BTreeMap<String, String>>;

/// Saved settings plus secrets: everything a delivery needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifyConfig {
    /// Non-secret settings.
    pub prefs: NotifyPrefs,
    /// Sealed-at-rest credentials.
    pub secrets: NotifySecrets,
}

/// One rejected field in a settings change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejection {
    /// `events.<id>`, `summaryHourUtc`, or `providers.<kind>.<field>`.
    pub field: String,
    /// Plain-language reason.
    pub reason: String,
}

fn reject(field: impl Into<String>, reason: impl Into<String>) -> Rejection {
    Rejection {
        field: field.into(),
        reason: reason.into(),
    }
}

/// A change to one provider: omitted parts are kept, `null` clears a field.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPatch {
    /// New on/off state.
    pub enabled: Option<bool>,
    /// Non-secret field changes.
    #[serde(default)]
    pub fields: BTreeMap<String, Option<String>>,
    /// Secret field changes.
    #[serde(default)]
    pub secrets: BTreeMap<String, Option<String>>,
}

/// A settings change from the console.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyPatch {
    /// Per-event switches, by wire name.
    #[serde(default)]
    pub events: BTreeMap<String, bool>,
    /// Per-provider changes, by wire name.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderPatch>,
    /// New daily-summary hour in UTC.
    pub summary_hour_utc: Option<u8>,
}

impl NotifyConfig {
    /// Applies `patch` to a copy of this config and validates the result.
    /// Every problem is reported at once; nothing is applied on error.
    ///
    /// # Errors
    /// Returns every [`Rejection`] when the patch names unknown events,
    /// providers, or fields, or leaves an enabled provider incomplete or
    /// malformed.
    pub fn patched(&self, patch: &NotifyPatch) -> Result<Self, Vec<Rejection>> {
        let mut next = self.clone();
        let mut rejected = Vec::new();

        for (name, enabled) in &patch.events {
            match NotifyEvent::parse(name) {
                Some(event) => {
                    next.prefs.events.insert(event, *enabled);
                }
                None => rejected.push(reject(format!("events.{name}"), "unknown event")),
            }
        }
        if let Some(hour) = patch.summary_hour_utc {
            if hour > 23 {
                rejected.push(reject("summaryHourUtc", "must be from 0 through 23"));
            } else {
                next.prefs.summary_hour_utc = hour;
            }
        }

        for (name, change) in &patch.providers {
            let Some(kind) = ProviderKind::parse(name) else {
                rejected.push(reject(format!("providers.{name}"), "unknown provider"));
                continue;
            };
            let prefs = next.prefs.providers.entry(kind).or_default();
            if let Some(enabled) = change.enabled {
                prefs.enabled = enabled;
            }
            for (field, value) in &change.fields {
                match kind.field(field) {
                    Some(spec) if !spec.secret => set_or_clear(&mut prefs.fields, field, value),
                    _ => rejected.push(reject(
                        format!("providers.{name}.{field}"),
                        "unknown setting for this provider",
                    )),
                }
            }
            let secrets = next.secrets.entry(kind).or_default();
            for (field, value) in &change.secrets {
                match kind.field(field) {
                    Some(spec) if spec.secret => set_or_clear(secrets, field, value),
                    _ => rejected.push(reject(
                        format!("providers.{name}.{field}"),
                        "unknown secret for this provider",
                    )),
                }
            }
        }
        next.secrets.retain(|_, fields| !fields.is_empty());

        for kind in ProviderKind::ALL {
            if next.prefs.provider_enabled(kind) {
                rejected.extend(kind.validate(&next.provider_fields(kind)));
            }
        }
        if rejected.is_empty() {
            Ok(next)
        } else {
            Err(rejected)
        }
    }

    /// One provider's fields and secrets merged, for validation and sending.
    pub fn provider_fields(&self, kind: ProviderKind) -> BTreeMap<String, String> {
        let mut merged = self
            .prefs
            .providers
            .get(&kind)
            .map(|prefs| prefs.fields.clone())
            .unwrap_or_default();
        if let Some(secrets) = self.secrets.get(&kind) {
            merged.extend(secrets.clone());
        }
        merged
    }

    /// The providers a notification would go to right now.
    pub fn enabled_providers(&self) -> Vec<ProviderKind> {
        ProviderKind::ALL
            .into_iter()
            .filter(|kind| self.prefs.provider_enabled(*kind))
            .collect()
    }

    /// Whether a notification of `event` would be delivered anywhere.
    pub fn wants(&self, event: NotifyEvent) -> bool {
        self.prefs.event_enabled(event) && !self.enabled_providers().is_empty()
    }
}

fn set_or_clear(map: &mut BTreeMap<String, String>, field: &str, value: &Option<String>) {
    match value.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => {
            map.insert(field.to_owned(), value.to_owned());
        }
        _ => {
            map.remove(field);
        }
    }
}

/// Last four characters of a secret, for "is this the one I saved?" checks.
fn hint(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return String::new();
    }
    chars[chars.len() - 4..].iter().collect()
}

/// The outcome of one delivery attempt sequence to one provider.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRecord {
    /// When delivery finished, UTC milliseconds.
    pub at_ms: i64,
    /// Provider wire name.
    pub provider: &'static str,
    /// Event wire name, or `"test"`.
    pub event: &'static str,
    /// Notification title.
    pub title: String,
    /// Whether the provider accepted it.
    pub ok: bool,
    /// Attempts made.
    pub attempts: u32,
    /// Failure reason, without secrets.
    pub detail: Option<String>,
}

#[derive(Debug, Default)]
struct Counters {
    dropped: AtomicU64,
    delivered: AtomicU64,
    failed: AtomicU64,
}

struct Inner {
    queue: mpsc::Sender<Notification>,
    config: RwLock<NotifyConfig>,
    client: reqwest::Client,
    counters: Counters,
    recent: Mutex<VecDeque<DeliveryRecord>>,
    available: bool,
}

/// Cloneable handle every event source uses. Cheap to clone; all clones feed
/// the same queue.
#[derive(Clone)]
pub struct Notifier(Arc<Inner>);

impl std::fmt::Debug for Notifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Notifier(redacted)")
    }
}

/// Drains the queue. Created with its [`Notifier`]; spawn [`NotifyWorker::run`]
/// once at startup.
pub struct NotifyWorker {
    queue: mpsc::Receiver<Notification>,
    notifier: Notifier,
}

/// Builds the one shared notification HTTP client.
///
/// # Errors
/// Returns the builder's error when TLS cannot initialise.
fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("veyra/", env!("CARGO_PKG_VERSION")))
        .build()
}

impl Notifier {
    /// Creates a notifier and the worker that must be spawned to drain it.
    /// `available` says whether settings can be saved (a credential vault and
    /// database exist); an unavailable notifier still accepts and drops
    /// notifications so callers never branch.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built.
    pub fn new(available: bool) -> Result<(Self, NotifyWorker), String> {
        let client = build_client().map_err(|error| error.without_url().to_string())?;
        let (queue, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let notifier = Self(Arc::new(Inner {
            queue,
            config: RwLock::new(NotifyConfig::default()),
            client,
            counters: Counters::default(),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_DELIVERIES)),
            available,
        }));
        let worker = NotifyWorker {
            queue: receiver,
            notifier: notifier.clone(),
        };
        Ok((notifier, worker))
    }

    /// A notifier with no worker: every notification is dropped. Used where
    /// notifications are not configured (and by tests).
    pub fn disabled() -> Self {
        let (queue, _receiver) = mpsc::channel(1);
        Self(Arc::new(Inner {
            queue,
            config: RwLock::new(NotifyConfig::default()),
            client: reqwest::Client::new(),
            counters: Counters::default(),
            recent: Mutex::new(VecDeque::new()),
            available: false,
        }))
    }

    /// Whether settings can be saved and notifications delivered.
    pub fn available(&self) -> bool {
        self.0.available
    }

    /// A snapshot of the current settings and secrets.
    pub fn config(&self) -> NotifyConfig {
        self.0
            .config
            .read()
            .map(|config| config.clone())
            .unwrap_or_default()
    }

    /// Replaces the settings every later delivery uses.
    pub fn set_config(&self, config: NotifyConfig) {
        if let Ok(mut current) = self.0.config.write() {
            *current = config;
        }
    }

    /// Whether a notification of `event` would go anywhere right now, so a
    /// source can skip building an expensive message.
    pub fn wants(&self, event: NotifyEvent) -> bool {
        self.available() && self.config().wants(event)
    }

    /// Queues a notification without waiting. Returns whether it was queued;
    /// a switched-off event, no enabled provider, or a full queue returns
    /// `false` (the last is counted as dropped).
    pub fn notify(&self, notification: Notification) -> bool {
        let Some(event) = notification.event else {
            return false;
        };
        if !self.wants(event) {
            return false;
        }
        match self.0.queue.try_send(notification) {
            Ok(()) => true,
            Err(error) => {
                self.0.counters.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(reason = %error, "notification dropped");
                false
            }
        }
    }

    /// Sends a test message to one provider now, bypassing the queue and the
    /// event switches, using its saved settings.
    ///
    /// # Errors
    /// Returns a secret-free reason when the provider is incomplete or
    /// rejects the message.
    pub async fn test(&self, kind: ProviderKind) -> Result<(), String> {
        let config = self.config();
        let fields = config.provider_fields(kind);
        if let Some(problem) = kind.validate(&fields).into_iter().next() {
            return Err(format!("{}: {}", problem.field, problem.reason));
        }
        let record = self.deliver(kind, &fields, &Notification::test()).await;
        match record.detail {
            None if record.ok => Ok(()),
            detail => Err(detail.unwrap_or_else(|| "delivery failed".to_owned())),
        }
    }

    /// Delivers to one provider with retries, recording the outcome.
    async fn deliver(
        &self,
        kind: ProviderKind,
        fields: &BTreeMap<String, String>,
        notification: &Notification,
    ) -> DeliveryRecord {
        let mut attempts = 0;
        let outcome = loop {
            attempts += 1;
            match providers::send(kind, &self.0.client, fields, notification).await {
                Ok(()) => break Ok(()),
                Err(error) if error.transient() && attempts < MAX_ATTEMPTS => {
                    let wait = RETRY_BASE * 2u32.pow(attempts);
                    actix_web::rt::time::sleep(wait).await;
                }
                Err(error) => break Err(error.to_string()),
            }
        };
        let record = DeliveryRecord {
            at_ms: (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            provider: kind.as_str(),
            event: notification.event_name(),
            title: notification.title.clone(),
            ok: outcome.is_ok(),
            attempts,
            detail: outcome.err(),
        };
        if record.ok {
            self.0.counters.delivered.fetch_add(1, Ordering::Relaxed);
        } else {
            self.0.counters.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                provider = record.provider,
                event = record.event,
                detail = record.detail.as_deref().unwrap_or(""),
                "notification delivery failed"
            );
        }
        if let Ok(mut recent) = self.0.recent.lock() {
            if recent.len() == RECENT_DELIVERIES {
                recent.pop_back();
            }
            recent.push_front(record.clone());
        }
        record
    }

    /// Fans one notification out to every enabled provider concurrently.
    async fn fan_out(&self, notification: Notification) {
        let config = self.config();
        let notification = Arc::new(notification);
        let mut deliveries = Vec::new();
        for kind in config.enabled_providers() {
            let notifier = self.clone();
            let fields = config.provider_fields(kind);
            let notification = notification.clone();
            deliveries.push(actix_web::rt::spawn(async move {
                notifier.deliver(kind, &fields, &notification).await
            }));
        }
        for delivery in deliveries {
            if let Err(error) = delivery.await {
                tracing::warn!(%error, "notification delivery task failed");
            }
        }
    }

    /// The console's view: settings with secrets redacted, queue counters,
    /// and recent deliveries.
    pub fn view(&self) -> Value {
        let config = self.config();
        let events: serde_json::Map<String, Value> = NotifyEvent::ALL
            .into_iter()
            .map(|event| {
                (
                    event.as_str().to_owned(),
                    Value::Bool(config.prefs.event_enabled(event)),
                )
            })
            .collect();
        let providers: serde_json::Map<String, Value> = ProviderKind::ALL
            .into_iter()
            .map(|kind| {
                let prefs = config
                    .prefs
                    .providers
                    .get(&kind)
                    .cloned()
                    .unwrap_or_default();
                let saved = config.secrets.get(&kind);
                let secrets: serde_json::Map<String, Value> = kind
                    .fields()
                    .iter()
                    .filter(|spec| spec.secret)
                    .map(|spec| {
                        let value = saved.and_then(|secrets| secrets.get(spec.name));
                        (
                            spec.name.to_owned(),
                            json!({
                                "set": value.is_some(),
                                "hint": value.map(|secret| hint(secret)),
                            }),
                        )
                    })
                    .collect();
                (
                    kind.as_str().to_owned(),
                    json!({
                        "enabled": prefs.enabled,
                        "fields": prefs.fields,
                        "secrets": secrets,
                    }),
                )
            })
            .collect();
        let counters = &self.0.counters;
        let recent: Vec<DeliveryRecord> = self
            .0
            .recent
            .lock()
            .map(|recent| recent.iter().cloned().collect())
            .unwrap_or_default();
        json!({
            "available": self.available(),
            "summaryHourUtc": config.prefs.summary_hour_utc,
            "events": events,
            "providers": providers,
            "status": {
                // Waiting for the worker right now, not a running total.
                "pending": QUEUE_CAPACITY.saturating_sub(self.0.queue.capacity()),
                "dropped": counters.dropped.load(Ordering::Relaxed),
                "delivered": counters.delivered.load(Ordering::Relaxed),
                "failed": counters.failed.load(Ordering::Relaxed),
            },
            "recent": recent,
        })
    }
}

impl NotifyWorker {
    /// Drains the queue for the life of the process, keeping at most
    /// [`MAX_IN_FLIGHT`] notifications in delivery. It ends only if the queue
    /// closes.
    pub async fn run(mut self) {
        let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
        while let Some(notification) = self.queue.recv().await {
            let Ok(permit) = permits.clone().acquire_owned().await else {
                break;
            };
            let notifier = self.notifier.clone();
            actix_web::rt::spawn(async move {
                notifier.fan_out(notification).await;
                drop(permit);
            });
        }
    }
}

#[cfg(test)]
mod tests;
