//! Per-candidate model cooldowns: a small circuit breaker.
//!
//! A candidate that fails in a way a *different* model could avoid — an
//! exhausted balance, a model the account may not use, revoked authorization,
//! a rate limit, an overloaded upstream, or an answer that does not satisfy the
//! schema — is taken out of rotation for a reason-based period. The period
//! doubles on every repeat up to a per-reason cap, so a model that stays dead
//! costs one request per cap window instead of one per tick.
//!
//! While a candidate cools, every chain skips it without sending a request.
//! When the period expires, the next call is a single half-open probe: success
//! clears the entry, failure extends it by the next backoff step, and callers
//! arriving while the probe is in flight keep skipping it.
//!
//! The registry is keyed by `(provider, model)` and lives in `AppState`, so it
//! survives engine rebuilds; it is cleared explicitly when the provider,
//! credential, model list, or subscription connection changes.
//!
//! A transport fault says nothing about a model but a lot about its host: an
//! unreachable provider would fail every candidate it serves the same way. So
//! a transport fault holds *all* of that host's candidates for the route's
//! tier under the short `unreachable` schedule, and the first candidate on
//! that host to answer again releases every `unreachable` hold on it.
//!
//! Only provider names, model identifiers, and reason categories are stored
//! or logged — never provider bodies or credentials.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::model::ModelProvider;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3_600);

/// Why a candidate is cooling down. Each reason has its own schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownReason {
    /// The account has no balance for this model (HTTP 402 or a credit
    /// message). 30 minutes, doubling, capped at 4 hours.
    InsufficientCredits,
    /// The provider refused the model or request outright (404, 400, and any
    /// other unclassified status) — for example a model the account's
    /// allowed-provider policy excludes. 60 minutes, capped at 12 hours.
    ProviderRejected,
    /// The credential was refused (401/403) or a subscription needs to be
    /// reconnected. 60 minutes, capped at 12 hours.
    Unauthorized,
    /// HTTP 429. Honours the provider's `Retry-After` hint when present,
    /// otherwise 2 minutes doubling; capped at 30 minutes.
    RateLimited,
    /// 5xx, 408/425, or an explicit overload. 1 minute, capped at 15 minutes.
    Overloaded,
    /// The provider answered, but not with anything usable: the structured
    /// answer was missing, malformed, or failed the schema. 5 minutes, capped
    /// at 30 minutes.
    InvalidResponse,
    /// The candidate's host could not be reached (connection, DNS, TLS, or a
    /// timeout), which holds every candidate on that host. 1 minute, capped
    /// at 5 minutes.
    Unreachable,
}

impl CooldownReason {
    /// Stable category name used in status output and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InsufficientCredits => "insufficient_credits",
            Self::ProviderRejected => "provider_rejected",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::Overloaded => "overloaded",
            Self::InvalidResponse => "invalid_response",
            Self::Unreachable => "unreachable",
        }
    }

    /// Cooldown after the first failure with this reason.
    pub fn base(self) -> Duration {
        match self {
            Self::InsufficientCredits => 30 * MINUTE,
            Self::ProviderRejected | Self::Unauthorized => HOUR,
            Self::RateLimited => 2 * MINUTE,
            Self::Overloaded | Self::Unreachable => MINUTE,
            Self::InvalidResponse => 5 * MINUTE,
        }
    }

    /// Longest cooldown this reason can reach, however often it repeats.
    pub fn cap(self) -> Duration {
        match self {
            Self::InsufficientCredits => 4 * HOUR,
            Self::ProviderRejected | Self::Unauthorized => 12 * HOUR,
            Self::RateLimited | Self::InvalidResponse => 30 * MINUTE,
            Self::Overloaded => 15 * MINUTE,
            Self::Unreachable => 5 * MINUTE,
        }
    }

    /// Classifies a non-success HTTP status.
    pub fn from_status(status: u16) -> Self {
        match status {
            401 | 403 => Self::Unauthorized,
            402 => Self::InsufficientCredits,
            429 => Self::RateLimited,
            408 | 425 | 500..=599 => Self::Overloaded,
            _ => Self::ProviderRejected,
        }
    }
}

impl fmt::Display for CooldownReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One failure that puts a candidate on cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CooldownFailure {
    reason: CooldownReason,
    retry_after: Option<Duration>,
}

impl CooldownFailure {
    /// A failure without a provider retry hint.
    pub fn new(reason: CooldownReason) -> Self {
        Self {
            reason,
            retry_after: None,
        }
    }

    /// A rate limit, with the provider's `Retry-After` hint when it sent one.
    pub fn rate_limited(retry_after: Option<Duration>) -> Self {
        Self {
            reason: CooldownReason::RateLimited,
            retry_after,
        }
    }

    /// Classifies a non-success HTTP status; `retry_after` only matters for
    /// a 429.
    pub fn from_status(status: u16, retry_after: Option<Duration>) -> Self {
        match CooldownReason::from_status(status) {
            CooldownReason::RateLimited => Self::rate_limited(retry_after),
            reason => Self::new(reason),
        }
    }

    /// Why the candidate failed.
    pub fn reason(&self) -> CooldownReason {
        self.reason
    }

    /// The provider's retry hint, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

/// Cooldown for the `failures`-th consecutive failure (1-based).
///
/// The period starts at [`CooldownReason::base`] and doubles per repeat up to
/// [`CooldownReason::cap`]. A rate limit with a provider `Retry-After` hint
/// uses the hint instead, still bounded by the cap so one hostile header
/// cannot park a candidate for a day.
pub fn cooldown_period(failure: CooldownFailure, failures: u32) -> Duration {
    let reason = failure.reason;
    if reason == CooldownReason::RateLimited
        && let Some(hint) = failure.retry_after
    {
        return hint.min(reason.cap());
    }
    let doublings = failures.saturating_sub(1).min(31);
    reason
        .base()
        .saturating_mul(2_u32.saturating_pow(doublings))
        .min(reason.cap())
}

/// Wall-clock source, injectable so cooldown tests are deterministic.
pub type CooldownClock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CandidateKey {
    provider: &'static str,
    model: String,
}

#[derive(Debug, Clone)]
struct Entry {
    reason: CooldownReason,
    until: SystemTime,
    failures: u32,
    /// A half-open probe is in flight; everyone else keeps skipping.
    probing: bool,
}

/// A candidate the registry is currently holding back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cooling {
    /// Why it is cooling.
    pub reason: CooldownReason,
    /// Earliest instant the next attempt may be made.
    pub until: SystemTime,
}

/// Non-sensitive view of one registry entry, as `/status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CooldownEntry {
    /// Provider identifier (`codex` for the ChatGPT subscription).
    pub provider: String,
    /// Model identifier as sent to that provider.
    pub model: String,
    /// Category of the most recent failure.
    pub reason: String,
    /// Epoch milliseconds when the next attempt becomes a half-open probe.
    /// A value in the past means the probe is due on the next call.
    pub until_ms: u64,
    /// Consecutive failures behind the current backoff step.
    pub failures: u32,
}

/// Whether a candidate may be attempted right now.
#[derive(Debug)]
pub enum Admission {
    /// Attempt it, then resolve the ticket with the outcome.
    Ready(AttemptTicket),
    /// Skip it; no request may be sent.
    Cooling(Cooling),
}

/// Shared per-candidate cooldown registry.
///
/// Cloning shares the same registry, which is how it survives engine rebuilds.
#[derive(Clone)]
pub struct CooldownRegistry {
    entries: Arc<Mutex<HashMap<CandidateKey, Entry>>>,
    clock: CooldownClock,
}

impl fmt::Debug for CooldownRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CooldownRegistry")
            .field("entries", &self.lock().len())
            .finish_non_exhaustive()
    }
}

impl Default for CooldownRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CooldownRegistry {
    /// An empty registry reading the system clock.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemTime::now))
    }

    /// An empty registry reading an injected clock.
    pub fn with_clock(clock: CooldownClock) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            clock,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<CandidateKey, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn now(&self) -> SystemTime {
        (self.clock)()
    }

    fn key(provider: ModelProvider, model: &str) -> CandidateKey {
        CandidateKey {
            provider: provider.as_str(),
            model: model.to_owned(),
        }
    }

    /// Decides whether `model` on `provider` may be attempted now.
    ///
    /// An expired entry admits exactly one caller as a half-open probe; later
    /// callers keep skipping it until that probe resolves.
    pub fn admit(&self, provider: ModelProvider, model: &str) -> Admission {
        let key = Self::key(provider, model);
        let now = self.now();
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&key) else {
            return Admission::Ready(AttemptTicket::new(self.clone(), key, false));
        };
        if entry.probing || now < entry.until {
            return Admission::Cooling(Cooling {
                reason: entry.reason,
                until: entry.until.max(now),
            });
        }
        entry.probing = true;
        tracing::info!(
            provider = key.provider,
            model = %key.model,
            reason = entry.reason.as_str(),
            failures = entry.failures,
            "model cooldown expired; sending one half-open probe"
        );
        drop(entries);
        Admission::Ready(AttemptTicket::new(self.clone(), key, true))
    }

    /// Reports whether a candidate would be skipped, without claiming the
    /// half-open probe. Used to refuse a request before any cost is incurred.
    pub fn peek(&self, provider: ModelProvider, model: &str) -> Option<Cooling> {
        let key = Self::key(provider, model);
        let now = self.now();
        let entries = self.lock();
        let entry = entries.get(&key)?;
        (entry.probing || now < entry.until).then(|| Cooling {
            reason: entry.reason,
            until: entry.until.max(now),
        })
    }

    /// Removes every entry, returning how many were cleared.
    pub fn clear(&self, cause: &str) -> usize {
        let cleared = {
            let mut entries = self.lock();
            let cleared = entries.len();
            entries.clear();
            cleared
        };
        if cleared > 0 {
            tracing::info!(cleared, cause, "model cooldowns cleared");
        }
        cleared
    }

    /// Every entry, soonest retry first.
    pub fn snapshot(&self) -> Vec<CooldownEntry> {
        let mut entries: Vec<CooldownEntry> = self
            .lock()
            .iter()
            .map(|(key, entry)| CooldownEntry {
                provider: key.provider.to_owned(),
                model: key.model.clone(),
                reason: entry.reason.as_str().to_owned(),
                until_ms: epoch_millis(entry.until),
                failures: entry.failures,
            })
            .collect();
        entries.sort_by(|left, right| {
            left.until_ms
                .cmp(&right.until_ms)
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.model.cmp(&right.model))
        });
        entries
    }

    /// Number of candidates with an entry.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no candidate has an entry.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Holds a candidate that was not attempted because its host just proved
    /// unreachable, at the host's current backoff step (`failures`).
    ///
    /// A candidate already cooling (or being probed) for its own reason keeps
    /// that longer, more specific hold.
    pub fn hold_unreachable(&self, provider: ModelProvider, model: &str, failures: u32) {
        let key = Self::key(provider, model);
        let now = self.now();
        let failure = CooldownFailure::new(CooldownReason::Unreachable);
        let failures = failures.max(1);
        let until = now
            .checked_add(cooldown_period(failure, failures))
            .unwrap_or(now);
        let mut entries = self.lock();
        match entries.get_mut(&key) {
            Some(entry) if entry.probing || now < entry.until => {}
            Some(entry) => {
                entry.reason = CooldownReason::Unreachable;
                entry.until = until;
                entry.failures = failures;
            }
            None => {
                entries.insert(
                    key,
                    Entry {
                        reason: CooldownReason::Unreachable,
                        until,
                        failures,
                        probing: false,
                    },
                );
            }
        }
    }

    /// Current backoff step of a candidate, zero without an entry.
    pub fn failures(&self, provider: ModelProvider, model: &str) -> u32 {
        self.lock()
            .get(&Self::key(provider, model))
            .map_or(0, |entry| entry.failures)
    }

    /// Releases every `unreachable` hold on a host that has just answered.
    /// Holds for any other reason are untouched.
    pub fn host_reachable(&self, provider: ModelProvider) -> usize {
        let released = {
            let mut entries = self.lock();
            let before = entries.len();
            entries.retain(|key, entry| {
                key.provider != provider.as_str() || entry.reason != CooldownReason::Unreachable
            });
            before - entries.len()
        };
        if released > 0 {
            tracing::info!(
                provider = provider.as_str(),
                released,
                "model host reachable again; unreachable holds released"
            );
        }
        released
    }

    fn resolve_success(&self, key: &CandidateKey) {
        let removed = self.lock().remove(key);
        if let Some(entry) = removed {
            tracing::info!(
                provider = key.provider,
                model = %key.model,
                previous_failures = entry.failures,
                "model candidate recovered; cooldown cleared"
            );
        }
    }

    fn resolve_failure(&self, key: &CandidateKey, probe: bool, failure: CooldownFailure) {
        let now = self.now();
        let mut entries = self.lock();
        let entry = entries.entry(key.clone()).or_insert(Entry {
            reason: failure.reason,
            until: now,
            failures: 0,
            probing: false,
        });
        // A second caller that was already in flight when another one cooled
        // this candidate is the same outage, not a repeat: it must not skip a
        // backoff step.
        let concurrent = !probe && entry.failures > 0 && now < entry.until;
        if !concurrent {
            entry.failures = entry.failures.saturating_add(1);
        }
        entry.reason = failure.reason;
        let period = cooldown_period(failure, entry.failures);
        let until = now.checked_add(period).unwrap_or(now);
        entry.until = if concurrent {
            entry.until.max(until)
        } else {
            until
        };
        if probe {
            entry.probing = false;
        }
        tracing::warn!(
            provider = key.provider,
            model = %key.model,
            reason = entry.reason.as_str(),
            failures = entry.failures,
            cooldown_secs = period.as_secs(),
            until_ms = epoch_millis(entry.until),
            "model candidate cooling down"
        );
    }

    fn release_probe(&self, key: &CandidateKey) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.probing = false;
        }
    }
}

/// Permission to attempt one candidate. Resolve it with the outcome; dropping
/// it unresolved (a cancelled request) releases a half-open probe so the
/// candidate is not stranded.
pub struct AttemptTicket {
    registry: CooldownRegistry,
    key: CandidateKey,
    probe: bool,
    resolved: bool,
}

impl fmt::Debug for AttemptTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttemptTicket")
            .field("provider", &self.key.provider)
            .field("model", &self.key.model)
            .field("probe", &self.probe)
            .finish()
    }
}

impl AttemptTicket {
    fn new(registry: CooldownRegistry, key: CandidateKey, probe: bool) -> Self {
        Self {
            registry,
            key,
            probe,
            resolved: false,
        }
    }

    /// Whether this attempt is the half-open probe after a cooldown.
    pub fn is_probe(&self) -> bool {
        self.probe
    }

    /// The candidate answered: any cooldown entry is cleared.
    pub fn succeed(mut self) {
        self.resolved = true;
        self.registry.resolve_success(&self.key);
    }

    /// The candidate failed in a way worth cooling it down for.
    pub fn fail(mut self, failure: CooldownFailure) {
        self.resolved = true;
        self.registry
            .resolve_failure(&self.key, self.probe, failure);
    }

    /// The attempt said nothing about the candidate: no cooldown is recorded,
    /// and a half-open probe is released.
    pub fn release(mut self) {
        self.resolved = true;
        if self.probe {
            self.registry.release_probe(&self.key);
        }
    }
}

impl Drop for AttemptTicket {
    fn drop(&mut self) {
        if !self.resolved && self.probe {
            self.registry.release_probe(&self.key);
        }
    }
}

/// Epoch milliseconds, or zero for instants before the epoch.
pub(crate) fn epoch_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// RFC 3339 UTC rendering (second precision) for operator-facing messages.
pub(crate) fn format_utc(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    match time::OffsetDateTime::from_unix_timestamp(seconds) {
        Ok(moment) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            moment.year(),
            u8::from(moment.month()),
            moment.day(),
            moment.hour(),
            moment.minute(),
            moment.second()
        ),
        Err(_) => format!("epoch+{seconds}s"),
    }
}

#[cfg(test)]
pub(crate) mod test_clock {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::CooldownClock;

    /// Manually advanced clock for deterministic cooldown tests.
    #[derive(Debug, Clone)]
    pub(crate) struct ManualClock(Arc<AtomicU64>);

    impl ManualClock {
        /// Starts at a fixed, readable instant: 2026-01-01T00:00:00Z.
        pub(crate) fn new() -> Self {
            Self(Arc::new(AtomicU64::new(1_767_225_600_000)))
        }

        pub(crate) fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_millis(self.0.load(Ordering::SeqCst))
        }

        pub(crate) fn advance(&self, by: Duration) {
            let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
            self.0.fetch_add(millis, Ordering::SeqCst);
        }

        pub(crate) fn clock(&self) -> CooldownClock {
            let source = self.clone();
            Arc::new(move || source.now())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_clock::ManualClock;
    use super::*;

    fn registry() -> (CooldownRegistry, ManualClock) {
        let clock = ManualClock::new();
        (CooldownRegistry::with_clock(clock.clock()), clock)
    }

    fn ready(registry: &CooldownRegistry, model: &str) -> AttemptTicket {
        match registry.admit(ModelProvider::OpenRouter, model) {
            Admission::Ready(ticket) => ticket,
            Admission::Cooling(cooling) => panic!("{model} unexpectedly cooling: {cooling:?}"),
        }
    }

    fn cooling(registry: &CooldownRegistry, model: &str) -> Cooling {
        match registry.admit(ModelProvider::OpenRouter, model) {
            Admission::Cooling(cooling) => cooling,
            Admission::Ready(ticket) => panic!("{model} unexpectedly admitted: {ticket:?}"),
        }
    }

    #[test]
    fn each_reason_has_its_documented_schedule() {
        use CooldownReason::*;
        for (reason, base_mins, cap_mins, name) in [
            (InsufficientCredits, 30, 240, "insufficient_credits"),
            (ProviderRejected, 60, 720, "provider_rejected"),
            (Unauthorized, 60, 720, "unauthorized"),
            (RateLimited, 2, 30, "rate_limited"),
            (Overloaded, 1, 15, "overloaded"),
            (InvalidResponse, 5, 30, "invalid_response"),
            (Unreachable, 1, 5, "unreachable"),
        ] {
            assert_eq!(reason.base(), Duration::from_secs(base_mins * 60), "{name}");
            assert_eq!(reason.cap(), Duration::from_secs(cap_mins * 60), "{name}");
            assert_eq!(reason.as_str(), name);
            assert_eq!(reason.to_string(), name);
            assert_eq!(
                cooldown_period(CooldownFailure::new(reason), 1),
                reason.base()
            );
        }
    }

    #[test]
    fn statuses_map_to_reasons() {
        use CooldownReason::*;
        for (status, reason) in [
            (400, ProviderRejected),
            (404, ProviderRejected),
            (410, ProviderRejected),
            (422, ProviderRejected),
            (401, Unauthorized),
            (403, Unauthorized),
            (402, InsufficientCredits),
            (429, RateLimited),
            (408, Overloaded),
            (500, Overloaded),
            (503, Overloaded),
            (529, Overloaded),
        ] {
            assert_eq!(CooldownReason::from_status(status), reason, "{status}");
        }
        let limited = CooldownFailure::from_status(429, Some(Duration::from_secs(9)));
        assert_eq!(limited.reason(), RateLimited);
        assert_eq!(limited.retry_after(), Some(Duration::from_secs(9)));
        // A hint on anything but a rate limit is irrelevant and dropped.
        assert_eq!(
            CooldownFailure::from_status(503, Some(Duration::from_secs(9))).retry_after(),
            None
        );
    }

    #[test]
    fn backoff_doubles_per_repeat_and_stops_at_the_cap() {
        let credits = CooldownFailure::new(CooldownReason::InsufficientCredits);
        let minutes = |failures| cooldown_period(credits, failures).as_secs() / 60;
        assert_eq!(
            [minutes(1), minutes(2), minutes(3), minutes(4), minutes(5)],
            [30, 60, 120, 240, 240]
        );
        // A huge repeat count saturates instead of overflowing.
        assert_eq!(
            cooldown_period(credits, u32::MAX),
            Duration::from_secs(4 * 3_600)
        );

        let rejected = CooldownFailure::new(CooldownReason::ProviderRejected);
        assert_eq!(
            cooldown_period(rejected, 5),
            Duration::from_secs(12 * 3_600)
        );
        let overloaded = CooldownFailure::new(CooldownReason::Overloaded);
        assert_eq!(cooldown_period(overloaded, 3), Duration::from_secs(4 * 60));
        assert_eq!(cooldown_period(overloaded, 9), Duration::from_secs(15 * 60));
    }

    #[test]
    fn retry_after_is_honoured_but_capped() {
        let hinted = CooldownFailure::rate_limited(Some(Duration::from_secs(90)));
        assert_eq!(cooldown_period(hinted, 1), Duration::from_secs(90));
        assert_eq!(
            cooldown_period(hinted, 4),
            Duration::from_secs(90),
            "a provider hint is used as given, not multiplied"
        );
        let hostile = CooldownFailure::rate_limited(Some(Duration::from_secs(86_400)));
        assert_eq!(cooldown_period(hostile, 1), Duration::from_secs(30 * 60));
        let unhinted = CooldownFailure::rate_limited(None);
        assert_eq!(cooldown_period(unhinted, 1), Duration::from_secs(120));
        assert_eq!(cooldown_period(unhinted, 2), Duration::from_secs(240));
        assert_eq!(cooldown_period(unhinted, 6), Duration::from_secs(30 * 60));
    }

    #[test]
    fn a_failure_cools_the_candidate_until_the_period_passes() {
        let (registry, clock) = registry();
        ready(&registry, "vendor/a").fail(CooldownFailure::new(CooldownReason::Overloaded));

        let held = cooling(&registry, "vendor/a");
        assert_eq!(held.reason, CooldownReason::Overloaded);
        assert_eq!(held.until, clock.now() + Duration::from_secs(60));
        assert_eq!(
            registry.peek(ModelProvider::OpenRouter, "vendor/a"),
            Some(held)
        );
        // Another model and the same model on another provider are unaffected.
        ready(&registry, "vendor/b").release();
        assert!(matches!(
            registry.admit(ModelProvider::DeepSeek, "vendor/a"),
            Admission::Ready(_)
        ));

        clock.advance(Duration::from_secs(59));
        cooling(&registry, "vendor/a");
        clock.advance(Duration::from_secs(1));
        assert_eq!(registry.peek(ModelProvider::OpenRouter, "vendor/a"), None);
    }

    #[test]
    fn an_expired_cooldown_admits_exactly_one_half_open_probe() {
        let (registry, clock) = registry();
        let credits = CooldownFailure::new(CooldownReason::InsufficientCredits);
        ready(&registry, "vendor/a").fail(credits);
        clock.advance(Duration::from_secs(30 * 60));

        let probe = ready(&registry, "vendor/a");
        assert!(probe.is_probe());
        // While the probe is in flight, everyone else keeps skipping.
        let held = cooling(&registry, "vendor/a");
        assert_eq!(held.until, clock.now(), "a probe in flight is due now");
        assert!(
            registry
                .peek(ModelProvider::OpenRouter, "vendor/a")
                .is_some()
        );

        // A failed probe doubles the next step.
        probe.fail(credits);
        let held = cooling(&registry, "vendor/a");
        assert_eq!(held.until, clock.now() + Duration::from_secs(60 * 60));
        assert_eq!(registry.snapshot()[0].failures, 2);

        // A successful probe clears the entry entirely.
        clock.advance(Duration::from_secs(60 * 60));
        let probe = ready(&registry, "vendor/a");
        probe.succeed();
        assert!(registry.is_empty());
        let fresh = ready(&registry, "vendor/a");
        assert!(!fresh.is_probe());
        fresh.fail(credits);
        assert_eq!(
            registry.snapshot()[0].failures,
            1,
            "recovery resets the backoff"
        );
    }

    #[test]
    fn a_released_or_dropped_probe_is_not_stranded() {
        let (registry, clock) = registry();
        ready(&registry, "vendor/a").fail(CooldownFailure::new(CooldownReason::Overloaded));
        clock.advance(Duration::from_secs(60));

        // A released probe records nothing: the entry stays expired, so the
        // next call probes again.
        ready(&registry, "vendor/a").release();
        let probe = ready(&registry, "vendor/a");
        assert!(probe.is_probe());
        drop(probe);
        assert!(ready(&registry, "vendor/a").is_probe());
        assert_eq!(
            registry.snapshot()[0].failures,
            1,
            "no failure was recorded"
        );
    }

    #[test]
    fn an_unreachable_host_holds_its_candidates_briefly_and_releases_them_together() {
        let (registry, clock) = registry();
        let unreachable = CooldownFailure::new(CooldownReason::Unreachable);
        let minutes = |failures| cooldown_period(unreachable, failures).as_secs() / 60;
        assert_eq!(
            [minutes(1), minutes(2), minutes(3), minutes(4)],
            [1, 2, 4, 5]
        );

        // vendor/specific already cools for its own, longer reason.
        ready(&registry, "vendor/specific")
            .fail(CooldownFailure::new(CooldownReason::ProviderRejected));
        ready(&registry, "vendor/a").fail(unreachable);
        registry.hold_unreachable(ModelProvider::OpenRouter, "vendor/b", 1);
        registry.hold_unreachable(ModelProvider::OpenRouter, "vendor/specific", 1);
        assert_eq!(registry.failures(ModelProvider::OpenRouter, "vendor/a"), 1);
        assert_eq!(
            registry.failures(ModelProvider::OpenRouter, "vendor/none"),
            0
        );
        let held = cooling(&registry, "vendor/b");
        assert_eq!(held.reason, CooldownReason::Unreachable);
        assert_eq!(held.until, clock.now() + Duration::from_secs(60));
        assert_eq!(
            cooling(&registry, "vendor/specific").reason,
            CooldownReason::ProviderRejected,
            "a specific hold is not shortened by a host hold"
        );

        // An expired hold takes the host's current step.
        clock.advance(Duration::from_secs(60));
        registry.hold_unreachable(ModelProvider::OpenRouter, "vendor/b", 3);
        assert_eq!(
            cooling(&registry, "vendor/b").until,
            clock.now() + Duration::from_secs(4 * 60)
        );
        assert_eq!(registry.failures(ModelProvider::OpenRouter, "vendor/b"), 3);
        // An in-flight probe is left alone.
        let probe = ready(&registry, "vendor/a");
        registry.hold_unreachable(ModelProvider::OpenRouter, "vendor/a", 5);
        assert_eq!(registry.failures(ModelProvider::OpenRouter, "vendor/a"), 1);
        drop(probe);

        // Another provider's holds are not released by this one answering.
        registry.hold_unreachable(ModelProvider::Codex, "gpt-6-luna", 1);
        assert_eq!(registry.host_reachable(ModelProvider::OpenRouter), 2);
        assert_eq!(registry.host_reachable(ModelProvider::OpenRouter), 0);
        let left: Vec<String> = registry
            .snapshot()
            .into_iter()
            .map(|entry| format!("{}/{}={}", entry.provider, entry.model, entry.reason))
            .collect();
        assert_eq!(
            left,
            [
                "codex/gpt-6-luna=unreachable",
                "openrouter/vendor/specific=provider_rejected"
            ]
        );
    }

    #[test]
    fn a_concurrent_failure_does_not_skip_a_backoff_step() {
        let (registry, clock) = registry();
        let first = ready(&registry, "vendor/a");
        let second = ready(&registry, "vendor/a");
        let rejected = CooldownFailure::new(CooldownReason::ProviderRejected);
        first.fail(rejected);
        clock.advance(Duration::from_secs(5));
        second.fail(rejected);
        let entry = &registry.snapshot()[0];
        assert_eq!(entry.failures, 1, "the same outage counts once");
        assert_eq!(
            entry.until_ms,
            epoch_millis(clock.now() + Duration::from_secs(3_600)),
            "the later of the two periods applies"
        );
    }

    #[test]
    fn snapshots_are_sorted_camel_case_and_clearable() {
        let (registry, clock) = registry();
        ready(&registry, "vendor/slow")
            .fail(CooldownFailure::new(CooldownReason::ProviderRejected));
        ready(&registry, "vendor/quick").fail(CooldownFailure::new(CooldownReason::Overloaded));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(registry.len(), 2);
        assert_eq!(snapshot[0].model, "vendor/quick", "soonest retry first");
        let value = serde_json::to_value(&snapshot[0]).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "provider": "openrouter",
                "model": "vendor/quick",
                "reason": "overloaded",
                "untilMs": epoch_millis(clock.now() + Duration::from_secs(60)),
                "failures": 1
            })
        );
        assert!(format!("{registry:?}").contains("entries: 2"));

        assert_eq!(registry.clear("test"), 2);
        assert!(registry.snapshot().is_empty());
        assert_eq!(registry.clear("test"), 0);
        assert!(CooldownRegistry::default().is_empty());
    }

    #[test]
    fn clones_share_one_registry() {
        let (registry, _clock) = registry();
        let shared = registry.clone();
        ready(&registry, "vendor/a").fail(CooldownFailure::new(CooldownReason::InvalidResponse));
        assert_eq!(shared.len(), 1);
        shared.clear("test");
        assert!(registry.is_empty());
    }

    #[test]
    fn instants_render_for_operators() {
        let clock = ManualClock::new();
        assert_eq!(format_utc(clock.now()), "2026-01-01T00:00:00Z");
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_millis(UNIX_EPOCH), 0);
        assert_eq!(epoch_millis(clock.now()), 1_767_225_600_000);
    }
}
