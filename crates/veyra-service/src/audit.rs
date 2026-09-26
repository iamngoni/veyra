//! Audit-trail boundary.
//!
//! Durable history (commands, acknowledgements, broker snapshots,
//! reconciliation results) lives behind [`AuditTrail`], so storage swaps
//! without touching callers: the service ships a PostgreSQL implementation and
//! tests use an in-memory one. Writes are best-effort by design —
//! [`AuditRuntime::try_record`] logs failures instead of blocking a command —
//! while a configured-but-unusable database fails startup, so a missing trail
//! is never mistaken for an empty one.
//!
//! Routine reads are live-only: queueing and completing a read-only broker
//! command (ping, account snapshot, candles, symbol contract, order history)
//! reaches the in-memory feed and counters but is not stored. They are tens
//! of thousands of rows a day that no decision depends on, and storing them
//! slowed every write and read of the trail. Their failures, every order
//! command, and the snapshot's own `broker_snapshot` row stay durable.
//!
//! Reads are bounded: [`AuditQuery`] is a validated filter (kinds, symbol,
//! ticket, command ids, outcome, time window, row cap) whose semantics are
//! defined once by [`AuditQuery::matches`]. Storage may answer it with native
//! SQL, but every implementation must return the same rows, newest first.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::balance::BalancePoint;

/// Read-only broker commands whose queue and completion events are not
/// stored (see the module notes). Failures of these commands are stored.
pub const ROUTINE_READS: [&str; 5] = [
    "ping",
    "account_snapshot",
    "rates",
    "symbol_spec",
    "order_history",
];

/// Whether an event is a routine read's queue or completion, which the live
/// feed shows but the durable trail does not keep.
pub fn is_routine_read(kind: AuditKind, payload: &Value) -> bool {
    matches!(kind, AuditKind::CommandQueued | AuditKind::CommandCompleted)
        && payload
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|command| ROUTINE_READS.contains(&command))
}

/// Event categories written to the trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditKind {
    /// A command was queued for the terminal.
    CommandQueued,
    /// A command completed with a validated payload.
    CommandCompleted,
    /// A command failed, timed out, or was rejected by the terminal.
    CommandFailed,
    /// A validated broker snapshot was retained.
    BrokerSnapshot,
    /// A validated, broker-reported account balance was observed.
    BalanceObserved,
    /// The service process started with auditing enabled.
    ServiceStarted,
    /// Reconciliation found orders Veyra does not own.
    ReconciliationDrift,
    /// The autonomous loop evaluated one proposal.
    ProposalEvaluated,
    /// A Veyra-managed position disappeared from the book (closed at the venue).
    PositionClosed,
    /// The decision loop executed one read-only tool for the model.
    AgentToolCalled,
    /// One model turn: exactly what it was shown, and what it answered.
    AgentTurn,
    /// A failure worth surviving a restart — a panic, or an error that would
    /// otherwise exist only in the in-memory log ring.
    Failure,
    /// The live risk policy was replaced from the control surface.
    RiskPolicyUpdated,
    /// One or more live settings were changed from the control surface.
    RuntimeConfigUpdated,
}

impl AuditKind {
    /// Every category, in declaration order.
    pub const ALL: [Self; 14] = [
        Self::CommandQueued,
        Self::CommandCompleted,
        Self::CommandFailed,
        Self::BrokerSnapshot,
        Self::BalanceObserved,
        Self::ServiceStarted,
        Self::ReconciliationDrift,
        Self::ProposalEvaluated,
        Self::PositionClosed,
        Self::AgentToolCalled,
        Self::AgentTurn,
        Self::Failure,
        Self::RiskPolicyUpdated,
        Self::RuntimeConfigUpdated,
    ];

    /// Parses a stable wire name; unknown names are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Stable wire and column name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CommandQueued => "command_queued",
            Self::CommandCompleted => "command_completed",
            Self::CommandFailed => "command_failed",
            Self::BrokerSnapshot => "broker_snapshot",
            Self::BalanceObserved => "balance_observed",
            Self::ServiceStarted => "service_started",
            Self::ReconciliationDrift => "reconciliation_drift",
            Self::ProposalEvaluated => "proposal_evaluated",
            Self::PositionClosed => "position_closed",
            Self::AgentToolCalled => "agent_tool_called",
            Self::AgentTurn => "agent_turn",
            Self::Failure => "failure",
            Self::RiskPolicyUpdated => "risk_policy_updated",
            Self::RuntimeConfigUpdated => "runtime_config_updated",
        }
    }
}

/// One append-only audit entry.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    kind: AuditKind,
    payload: Value,
}

impl AuditEvent {
    /// Builds an event from a category and a bounded JSON payload.
    pub fn new(kind: AuditKind, payload: Value) -> Self {
        Self { kind, payload }
    }

    /// Event category.
    pub fn kind(&self) -> AuditKind {
        self.kind
    }

    /// Event payload.
    pub fn payload(&self) -> &Value {
        &self.payload
    }
}

/// One stored audit row as returned to operators.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    /// Row identity.
    pub id: String,
    /// Storage timestamp. [`AuditTrail::recent`] rows carry the store's own
    /// text form (PostgreSQL `timestamptz::text`); [`AuditTrail::query`] rows
    /// carry RFC 3339 UTC with milliseconds (see [`format_trail_time`]).
    pub at: String,
    /// Event category.
    pub kind: String,
    /// Event payload.
    pub payload: Value,
}

/// Supported audit storage implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditProvider {
    /// PostgreSQL via SQLx.
    Postgres,
}

impl AuditProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
        }
    }
}

impl fmt::Display for AuditProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Errors raised while reading or writing the audit trail.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// The store could not be built or reached.
    #[error("audit storage failed: {reason}")]
    Storage {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Largest page one filtered trail query may request.
pub const MAX_QUERY_ROWS: u32 = 200;

/// Largest set of command ids one filtered trail query may match.
pub const MAX_QUERY_COMMAND_IDS: usize = 64;

/// Longest `outcome` value a filter accepts.
const MAX_OUTCOME_CHARS: usize = 64;

/// Rows the generic [`AuditTrail::query`] fallback scans before filtering.
const QUERY_SCAN_ROWS: u32 = 10_000;

/// Why a trail filter was refused before it reached storage.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid audit query `{field}`: {reason}")]
pub struct AuditQueryError {
    /// Filter field that failed validation.
    pub field: &'static str,
    /// Why the value was rejected.
    pub reason: &'static str,
}

fn query_error(field: &'static str, reason: &'static str) -> AuditQueryError {
    AuditQueryError { field, reason }
}

/// Validated, bounded filter over the durable audit trail.
///
/// Every filter narrows the result (they combine with AND). `symbol` compares
/// ASCII case-insensitively with `payload.symbol`; `ticket` matches either
/// `payload.ticket` or `payload.result.ticket` (a completed open command
/// reports the new position's ticket there); `command_ids` matches
/// `payload.command_id`; the window is half-open, `since <= at < until`.
/// Results are newest first and never exceed [`AuditQuery::limit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditQuery {
    kinds: Vec<AuditKind>,
    symbol: Option<String>,
    ticket: Option<i64>,
    command_ids: Vec<String>,
    outcome: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: u32,
}

impl AuditQuery {
    /// Starts a filter over `kinds` returning at most `limit` rows.
    ///
    /// # Errors
    /// Returns [`AuditQueryError`] when no kind is given or `limit` is outside
    /// `1..=`[`MAX_QUERY_ROWS`].
    pub fn new(kinds: &[AuditKind], limit: u32) -> Result<Self, AuditQueryError> {
        if kinds.is_empty() {
            return Err(query_error("kinds", "must name at least one event kind"));
        }
        if limit == 0 || limit > MAX_QUERY_ROWS {
            return Err(query_error("limit", "must be from 1 through 200"));
        }
        let mut unique = Vec::with_capacity(kinds.len());
        for kind in kinds {
            if !unique.contains(kind) {
                unique.push(*kind);
            }
        }
        Ok(Self {
            kinds: unique,
            symbol: None,
            ticket: None,
            command_ids: Vec::new(),
            outcome: None,
            since_ms: None,
            until_ms: None,
            limit,
        })
    }

    /// Restricts rows to one instrument, compared ASCII case-insensitively.
    ///
    /// # Errors
    /// Returns [`AuditQueryError`] when the symbol is not a valid instrument.
    pub fn with_symbol(mut self, symbol: &str) -> Result<Self, AuditQueryError> {
        let symbol = crate::broker::Symbol::parse(symbol).map_err(|_| {
            query_error(
                "symbol",
                "must be 1-24 characters of letters, digits, '.', '_', '#', '+' or '-'",
            )
        })?;
        self.symbol = Some(symbol.as_str().to_ascii_uppercase());
        Ok(self)
    }

    /// Restricts rows to one venue ticket.
    ///
    /// # Errors
    /// Returns [`AuditQueryError`] when the ticket is not positive.
    pub fn with_ticket(mut self, ticket: i64) -> Result<Self, AuditQueryError> {
        if ticket <= 0 {
            return Err(query_error("ticket", "must be a positive integer"));
        }
        self.ticket = Some(ticket);
        Ok(self)
    }

    /// Restricts rows to events that carry one of `ids` as `command_id`.
    ///
    /// # Errors
    /// Returns [`AuditQueryError`] when the set is empty, larger than
    /// [`MAX_QUERY_COMMAND_IDS`], or contains a non-UUID value.
    pub fn with_command_ids(mut self, ids: &[String]) -> Result<Self, AuditQueryError> {
        if ids.is_empty() || ids.len() > MAX_QUERY_COMMAND_IDS {
            return Err(query_error("command_ids", "must list 1 through 64 ids"));
        }
        let mut canonical = Vec::with_capacity(ids.len());
        for id in ids {
            let id = uuid::Uuid::parse_str(id.trim())
                .map_err(|_| query_error("command_ids", "every id must be a UUID"))?
                .to_string();
            if !canonical.contains(&id) {
                canonical.push(id);
            }
        }
        self.command_ids = canonical;
        Ok(self)
    }

    /// Restricts rows to one recorded `payload.outcome`.
    ///
    /// # Errors
    /// Returns [`AuditQueryError`] when the outcome is empty, too long, or not
    /// a lowercase identifier.
    pub fn with_outcome(mut self, outcome: &str) -> Result<Self, AuditQueryError> {
        let valid = !outcome.is_empty()
            && outcome.chars().count() <= MAX_OUTCOME_CHARS
            && outcome
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            return Err(query_error(
                "outcome",
                "must be 1-64 characters of lowercase letters, digits or '_'",
            ));
        }
        self.outcome = Some(outcome.to_owned());
        Ok(self)
    }

    /// Restricts rows to the half-open window `since_ms <= at < until_ms`
    /// (Unix milliseconds); either bound may be open.
    ///
    /// # Errors
    /// Returns [`AuditQueryError`] when a bound is negative or the window is
    /// empty.
    pub fn with_window(
        mut self,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
    ) -> Result<Self, AuditQueryError> {
        if since_ms.is_some_and(|since| since < 0) || until_ms.is_some_and(|until| until < 0) {
            return Err(query_error(
                "window",
                "bounds must not precede the Unix epoch",
            ));
        }
        if let (Some(since), Some(until)) = (since_ms, until_ms)
            && since >= until
        {
            return Err(query_error("window", "since must be before until"));
        }
        self.since_ms = since_ms;
        self.until_ms = until_ms;
        Ok(self)
    }

    /// Event kinds to include, without duplicates.
    pub fn kinds(&self) -> &[AuditKind] {
        &self.kinds
    }

    /// Upper-cased instrument filter.
    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    /// Venue ticket filter.
    pub fn ticket(&self) -> Option<i64> {
        self.ticket
    }

    /// Canonical (lowercase, hyphenated) command ids; empty means no filter.
    pub fn command_ids(&self) -> &[String] {
        &self.command_ids
    }

    /// Recorded outcome filter.
    pub fn outcome(&self) -> Option<&str> {
        self.outcome.as_deref()
    }

    /// Inclusive lower bound, Unix milliseconds.
    pub fn since_ms(&self) -> Option<i64> {
        self.since_ms
    }

    /// Exclusive upper bound, Unix milliseconds.
    pub fn until_ms(&self) -> Option<i64> {
        self.until_ms
    }

    /// Maximum rows returned.
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// The single definition of which rows a filter selects. `at_ms` is the
    /// row's storage time; a row whose time is unknown never satisfies a
    /// time bound, so an unparseable timestamp cannot leak into a window.
    pub fn matches(&self, at_ms: Option<i64>, kind: &str, payload: &Value) -> bool {
        if !self
            .kinds
            .iter()
            .any(|candidate| candidate.as_str() == kind)
        {
            return false;
        }
        if let Some(symbol) = &self.symbol
            && payload
                .get("symbol")
                .and_then(Value::as_str)
                .map(str::to_ascii_uppercase)
                .as_deref()
                != Some(symbol.as_str())
        {
            return false;
        }
        if let Some(ticket) = self.ticket {
            let matches_ticket = |value: Option<&Value>| {
                value.is_some_and(|value| {
                    value.as_i64() == Some(ticket)
                        || value.as_str() == Some(ticket.to_string().as_str())
                })
            };
            if !matches_ticket(payload.get("ticket"))
                && !matches_ticket(
                    payload
                        .get("result")
                        .and_then(|result| result.get("ticket")),
                )
            {
                return false;
            }
        }
        if !self.command_ids.is_empty()
            && !payload
                .get("command_id")
                .and_then(Value::as_str)
                .is_some_and(|id| self.command_ids.iter().any(|candidate| candidate == id))
        {
            return false;
        }
        if let Some(outcome) = &self.outcome
            && payload.get("outcome").and_then(Value::as_str) != Some(outcome.as_str())
        {
            return false;
        }
        if self.since_ms.is_some() || self.until_ms.is_some() {
            let Some(at_ms) = at_ms else {
                return false;
            };
            if self.since_ms.is_some_and(|since| at_ms < since)
                || self.until_ms.is_some_and(|until| at_ms >= until)
            {
                return false;
            }
        }
        true
    }
}

/// Formats a storage instant as RFC 3339 UTC with milliseconds, the form
/// [`AuditTrail::query`] rows carry (for example `2026-09-24T06:46:09.120Z`).
pub fn format_trail_time(at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.millisecond()
    )
}

/// Parses a stored row time into Unix milliseconds.
///
/// Accepts RFC 3339 and PostgreSQL's `timestamptz::text` form
/// (`2026-09-24 06:46:09.123456+00`); anything else is `None`.
pub fn parse_trail_time(at: &str) -> Option<i64> {
    let mut text = at.trim().replacen(' ', "T", 1);
    // PostgreSQL prints whole-hour offsets as `+HH`; RFC 3339 needs `+HH:MM`.
    if let Some(sign) = text.rfind(['+', '-'])
        && sign > 10
        && text.len() - sign == 3
        && text[sign + 1..].chars().all(|c| c.is_ascii_digit())
    {
        text.push_str(":00");
    }
    let parsed = OffsetDateTime::parse(&text, &Rfc3339).ok()?;
    i64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Narrow contract every audit storage implements.
#[async_trait]
pub trait AuditTrail: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output.
    fn provider(&self) -> AuditProvider;

    /// Appends one event.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn record(&self, event: AuditEvent) -> Result<(), AuditError>;

    /// Returns the newest events, newest first.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn recent(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError>;

    /// Returns recent decision and position events, newest first. A busy EA
    /// emits many command acknowledgements between position reviews, so a
    /// general recent page cannot establish the latest holding rationale.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the trail cannot be read.
    async fn recent_decisions(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError> {
        let rows = self.recent(10_000).await?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                matches!(
                    row.kind.as_str(),
                    "proposal_evaluated" | "position_closed" | "command_failed"
                )
            })
            .take(limit as usize)
            .collect())
    }

    /// Returns rows selected by a validated filter, newest first, at most
    /// [`AuditQuery::limit`]. Rows carry RFC 3339 UTC times when the store
    /// can produce them.
    ///
    /// The default scans the newest 10,000 rows and applies
    /// [`AuditQuery::matches`], so every store shares one semantics; stores
    /// with a query language override it with an equivalent statement.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the trail cannot be read.
    async fn query(&self, query: &AuditQuery) -> Result<Vec<AuditRow>, AuditError> {
        let rows = self.recent(QUERY_SCAN_ROWS).await?;
        Ok(rows
            .into_iter()
            .filter(|row| query.matches(parse_trail_time(&row.at), &row.kind, &row.payload))
            .take(query.limit() as usize)
            .collect())
    }

    /// Reads actual balance observations for exactly one account, oldest first.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn balance_history(
        &self,
        _login: u64,
        _server: &str,
        _since_ms: u64,
    ) -> Result<Vec<BalancePoint>, AuditError> {
        Err(AuditError::Storage {
            reason: "balance history is unavailable".to_owned(),
        })
    }

    /// Deletes events older than `keep_days`, returning how many rows went.
    /// Zero keeps everything.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn prune(&self, keep_days: u32) -> Result<u64, AuditError>;
}

/// Bounded number of recent events retained in memory for live feeds.
const FEED_CAPACITY: usize = 512;

/// How often a long-poll feed checks the buffer for new events.
const FEED_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One event as seen by live feeds: a monotonic sequence number plus the
/// event contents. Sequence numbers start at 1 and survive buffer eviction as
/// a cursor; a client older than the buffer is simply handed the tail.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedEvent {
    /// Monotonic sequence number; `after` cursors compare against this.
    pub seq: u64,
    /// Unix milliseconds at record time.
    pub at_ms: u64,
    /// Event category.
    pub kind: AuditKind,
    /// Bounded JSON payload.
    pub payload: Value,
}

/// In-memory ring of the most recent events, shared by every feed reader.
#[derive(Debug)]
struct EventFeed {
    events: Mutex<VecDeque<FeedEvent>>,
    next_seq: AtomicU64,
}

impl EventFeed {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::with_capacity(FEED_CAPACITY)),
            next_seq: AtomicU64::new(1),
        }
    }

    fn publish(&self, kind: AuditKind, payload: &Value) {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        if let Ok(mut events) = self.events.lock() {
            events.push_back(FeedEvent {
                seq,
                at_ms,
                kind,
                payload: payload.clone(),
            });
            while events.len() > FEED_CAPACITY {
                events.pop_front();
            }
        }
    }

    /// Events strictly after `after`, oldest first, at most `limit`.
    fn after(&self, after: u64, limit: usize) -> Vec<FeedEvent> {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.seq > after)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Sequence number of the most recent event (zero before any).
    fn latest(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst).saturating_sub(1)
    }
}

/// Process-lifetime counters derived from the audit stream.
///
/// Cheap tallies for the operations surface: every recorded event bumps one
/// total, proposals also bump their outcome, and command events bump their
/// command kind. Counters reset with the process; the durable trail remains
/// the source of truth.
#[derive(Debug, Default)]
struct EventCounters {
    counts: Mutex<std::collections::BTreeMap<String, u64>>,
}

impl EventCounters {
    fn increment(&self, key: &str) {
        let mut counts = match self.counts.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *counts.entry(key.to_owned()).or_insert(0) += 1;
    }

    fn snapshot(&self) -> std::collections::BTreeMap<String, u64> {
        let counts = match self.counts.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        counts.clone()
    }

    fn tally(&self, kind: AuditKind, payload: &Value) {
        self.increment(&format!("event.{}", kind.as_str()));
        if kind == AuditKind::ProposalEvaluated
            && let Some(outcome) = payload.get("outcome").and_then(Value::as_str)
        {
            self.increment(&format!("proposal.{outcome}"));
        }
        if matches!(
            kind,
            AuditKind::CommandQueued | AuditKind::CommandCompleted | AuditKind::CommandFailed
        ) && let Some(command) = payload.get("kind").and_then(Value::as_str)
        {
            self.increment(&format!("command.{}.{}", kind.as_str(), command));
        }
    }
}

/// Active audit integration.
#[derive(Debug, Clone)]
pub struct AuditRuntime {
    trail: Arc<dyn AuditTrail>,
    feed: Arc<EventFeed>,
    counters: Arc<EventCounters>,
}

impl AuditRuntime {
    /// Wraps a storage implementation.
    pub fn new(trail: Arc<dyn AuditTrail>) -> Self {
        Self {
            trail,
            feed: Arc::new(EventFeed::new()),
            counters: Arc::new(EventCounters::default()),
        }
    }

    /// Counters since process start, keyed by `event.*`, `proposal.*`, and
    /// `command.*`.
    pub fn counters(&self) -> std::collections::BTreeMap<String, u64> {
        self.counters.snapshot()
    }

    /// Sequence number of the most recent recorded event (zero before any).
    pub fn feed_latest(&self) -> u64 {
        self.feed.latest()
    }

    /// Returns events after `after`, waiting up to `wait` for the first one
    /// when none is buffered yet. Long-poll friendly: the caller decides the
    /// window and always gets an answer.
    pub async fn feed_after(&self, after: u64, limit: usize, wait: Duration) -> Vec<FeedEvent> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let events = self.feed.after(after, limit);
            if !events.is_empty() {
                return events;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Vec::new();
            }
            actix_web::rt::time::sleep(FEED_POLL_INTERVAL.min(remaining)).await;
        }
    }

    /// Provider identifier of the active implementation.
    pub fn provider(&self) -> AuditProvider {
        self.trail.provider()
    }

    /// Storage implementation behind this runtime.
    pub fn trail(&self) -> &Arc<dyn AuditTrail> {
        &self.trail
    }

    /// Appends an event best-effort: failures are logged, never propagated, so
    /// an audit hiccup can not block or fail a command. The event also enters
    /// the in-memory feed for live readers, whether or not storage accepts it.
    /// Routine reads ([`is_routine_read`]) go to the feed and counters only.
    pub async fn try_record(&self, event: AuditEvent) {
        self.feed.publish(event.kind(), event.payload());
        self.counters.tally(event.kind(), event.payload());
        if is_routine_read(event.kind(), event.payload()) {
            return;
        }
        if let Err(error) = self.trail.record(event).await {
            tracing::warn!(%error, "audit write failed");
        }
    }

    /// Prunes the trail best-effort: failures are logged and reported as zero
    /// rows removed, so a storage hiccup never fails the maintenance loop.
    pub async fn try_prune(&self, keep_days: u32) -> u64 {
        match self.trail.prune(keep_days).await {
            Ok(deleted) => deleted,
            Err(error) => {
                tracing::warn!(%error, "audit prune failed");
                0
            }
        }
    }
}

/// In-memory audit trail.
///
/// Useful for tests and local experiments only: nothing in the production path
/// selects it, so a missing database is never mistaken for a durable one.
/// Every entry keeps its record time so filtered reads honour the same time
/// window semantics as PostgreSQL.
#[derive(Debug, Default)]
pub struct MemoryTrail {
    events: std::sync::Mutex<Vec<MemoryEntry>>,
}

/// One retained entry: insertion order, record time, and the event.
#[derive(Debug, Clone)]
struct MemoryEntry {
    seq: usize,
    at: OffsetDateTime,
    event: AuditEvent,
}

impl MemoryEntry {
    fn row(&self) -> AuditRow {
        AuditRow {
            id: format!("memory-{}", self.seq),
            at: format_trail_time(self.at),
            kind: self.event.kind().as_str().to_owned(),
            payload: self.event.payload().clone(),
        }
    }

    fn at_ms(&self) -> Option<i64> {
        i64::try_from(self.at.unix_timestamp_nanos() / 1_000_000).ok()
    }
}

impl MemoryTrail {
    /// Number of retained events.
    pub fn len(&self) -> usize {
        self.events.lock().map(|events| events.len()).unwrap_or(0)
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns every recorded event, oldest first.
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .map(|events| events.iter().map(|entry| entry.event.clone()).collect())
            .unwrap_or_default()
    }

    /// Appends one event with an explicit record time, for replaying a
    /// journal or building time-dependent fixtures.
    pub fn record_at(&self, at: OffsetDateTime, event: AuditEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = events.len();
        events.push(MemoryEntry { seq, at, event });
    }

    /// Entries newest first: record time descending, then insertion order
    /// descending, mirroring PostgreSQL's `order by at desc, id desc`.
    fn newest_first(&self) -> Vec<MemoryEntry> {
        let mut entries = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        entries.sort_by(|left, right| right.at.cmp(&left.at).then(right.seq.cmp(&left.seq)));
        entries
    }
}

#[async_trait]
impl AuditTrail for MemoryTrail {
    fn provider(&self) -> AuditProvider {
        AuditProvider::Postgres
    }

    async fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        self.record_at(OffsetDateTime::now_utc(), event);
        Ok(())
    }

    async fn prune(&self, _keep_days: u32) -> Result<u64, AuditError> {
        // The in-memory trail never prunes: tests assert on what was written.
        Ok(0)
    }

    async fn recent(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError> {
        Ok(self
            .newest_first()
            .iter()
            .take(limit as usize)
            .map(MemoryEntry::row)
            .collect())
    }

    async fn query(&self, query: &AuditQuery) -> Result<Vec<AuditRow>, AuditError> {
        Ok(self
            .newest_first()
            .iter()
            .filter(|entry| {
                query.matches(
                    entry.at_ms(),
                    entry.event.kind().as_str(),
                    entry.event.payload(),
                )
            })
            .take(query.limit() as usize)
            .map(MemoryEntry::row)
            .collect())
    }

    async fn balance_history(
        &self,
        login: u64,
        server: &str,
        since_ms: u64,
    ) -> Result<Vec<BalancePoint>, AuditError> {
        let events = self.events();
        let mut points: Vec<_> = events
            .iter()
            .filter(|event| event.kind() == AuditKind::BalanceObserved)
            .filter(|event| event.payload()["login"].as_u64() == Some(login))
            .filter(|event| event.payload()["server"].as_str() == Some(server))
            .filter_map(|event| {
                let at_ms = event.payload()["atMs"].as_u64()?;
                let balance = event.payload()["balance"].as_f64()?;
                (at_ms >= since_ms && balance.is_finite())
                    .then_some(BalancePoint { at_ms, balance })
            })
            .collect();
        points.sort_by_key(|point| point.at_ms);
        Ok(points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[actix_web::test]
    async fn balance_history_is_account_scoped_chronological_and_preserves_deficits() {
        let trail = MemoryTrail::default();
        for (login, at_ms, balance) in [
            (10, 300, -2.5),
            (11, 200, 500.0),
            (10, 100, 4.0),
            (10, 200, 0.0),
        ] {
            trail
                .record(AuditEvent::new(
                    AuditKind::BalanceObserved,
                    json!({"login": login, "server": "Broker-Real", "atMs": at_ms, "balance": balance}),
                ))
                .await
                .expect("record");
        }
        let points = trail
            .balance_history(10, "Broker-Real", 0)
            .await
            .expect("history");
        assert_eq!(
            points.iter().map(|point| point.at_ms).collect::<Vec<_>>(),
            vec![100, 200, 300]
        );
        assert_eq!(
            points.iter().map(|point| point.balance).collect::<Vec<_>>(),
            vec![4.0, 0.0, -2.5]
        );
        assert_eq!(
            trail.balance_history(10, "Other", 0).await.expect("other"),
            Vec::new()
        );
        assert_eq!(
            trail
                .balance_history(10, "Broker-Real", 201)
                .await
                .expect("window")
                .len(),
            1
        );
    }

    /// Storage that always fails, to prove writes never propagate.
    #[derive(Debug)]
    struct BrokenTrail;

    #[async_trait]
    impl AuditTrail for BrokenTrail {
        fn provider(&self) -> AuditProvider {
            AuditProvider::Postgres
        }

        async fn record(&self, _event: AuditEvent) -> Result<(), AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }

        async fn recent(&self, _limit: u32) -> Result<Vec<AuditRow>, AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }

        async fn prune(&self, _keep_days: u32) -> Result<u64, AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }
    }

    #[actix_web::test]
    async fn counters_tally_events_outcomes_and_commands() {
        use serde_json::json;

        let runtime = AuditRuntime::new(Arc::new(MemoryTrail::default()));
        assert!(runtime.counters().is_empty(), "nothing recorded yet");

        runtime
            .try_record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "held"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "held"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "queued"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::CommandQueued,
                json!({"kind": "open_order"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::CommandCompleted,
                json!({"kind": "open_order"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(AuditKind::BrokerSnapshot, json!({})))
            .await;

        let counters = runtime.counters();
        assert_eq!(counters["event.proposal_evaluated"], 3);
        assert_eq!(counters["proposal.held"], 2);
        assert_eq!(counters["proposal.queued"], 1);
        assert_eq!(counters["event.command_queued"], 1);
        assert_eq!(counters["command.command_queued.open_order"], 1);
        assert_eq!(counters["command.command_completed.open_order"], 1);
        assert_eq!(counters["event.broker_snapshot"], 1);
    }

    #[test]
    fn only_routine_read_queue_and_completion_events_are_live_only() {
        use serde_json::json;
        for command in ROUTINE_READS {
            assert!(is_routine_read(
                AuditKind::CommandQueued,
                &json!({"kind": command})
            ));
            assert!(is_routine_read(
                AuditKind::CommandCompleted,
                &json!({"kind": command})
            ));
            assert!(
                !is_routine_read(AuditKind::CommandFailed, &json!({"kind": command})),
                "failures are stored"
            );
        }
        for order in ["open_order", "close_order", "modify_order", "order_check"] {
            assert!(!is_routine_read(
                AuditKind::CommandCompleted,
                &json!({"kind": order})
            ));
        }
        assert!(!is_routine_read(
            AuditKind::BrokerSnapshot,
            &json!({"kind": "account_snapshot"})
        ));
        assert!(!is_routine_read(AuditKind::CommandCompleted, &json!({})));
    }

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(AuditKind::CommandQueued.as_str(), "command_queued");
        assert_eq!(AuditKind::CommandCompleted.as_str(), "command_completed");
        assert_eq!(AuditKind::CommandFailed.as_str(), "command_failed");
        assert_eq!(AuditKind::BrokerSnapshot.as_str(), "broker_snapshot");
        assert_eq!(AuditKind::ServiceStarted.as_str(), "service_started");
        assert_eq!(
            AuditKind::ReconciliationDrift.as_str(),
            "reconciliation_drift"
        );
        assert_eq!(AuditKind::ProposalEvaluated.as_str(), "proposal_evaluated");
        assert_eq!(AuditKind::PositionClosed.as_str(), "position_closed");
        assert_eq!(AuditProvider::Postgres.as_str(), "postgres");
        assert_eq!(AuditProvider::Postgres.to_string(), "postgres");
    }

    #[actix_web::test]
    async fn runtime_records_and_reads_through_the_trail() {
        let trail = Arc::new(MemoryTrail::default());
        let runtime = AuditRuntime::new(trail.clone());
        assert_eq!(runtime.provider(), AuditProvider::Postgres);

        runtime
            .try_record(AuditEvent::new(
                AuditKind::CommandQueued,
                json!({"command_id": "abc"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::BrokerSnapshot,
                json!({"orders": 0}),
            ))
            .await;

        assert_eq!(trail.len(), 2);
        assert_eq!(trail.events().len(), 2);
        assert!(!trail.is_empty());

        let rows = runtime.trail().recent(10).await.expect("readable");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "broker_snapshot");
        assert_eq!(rows[1].payload["command_id"], "abc");
    }

    #[actix_web::test]
    async fn decision_history_survives_a_burst_of_command_acknowledgements() {
        let trail = MemoryTrail::default();
        trail
            .record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "held", "rationale": "The bracket remains valid."}),
            ))
            .await
            .expect("record hold");
        for _ in 0..250 {
            trail
                .record(AuditEvent::new(AuditKind::CommandCompleted, json!({})))
                .await
                .expect("record acknowledgement");
        }
        let rows = trail.recent_decisions(35).await.expect("read decisions");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payload["outcome"], "held");
    }

    fn instant(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).expect("fixture time")
    }

    fn ms(text: &str) -> i64 {
        i64::try_from(instant(text).unix_timestamp_nanos() / 1_000_000).expect("range")
    }

    const OPEN: &str = "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10";
    const CLOSE: &str = "7c5a7e3a-4d3f-4c79-9f49-9d2f4a9a0c32";

    /// A trade lifecycle recorded out of order, plus noise.
    fn lifecycle(trail: &MemoryTrail) {
        for (at, kind, payload) in [
            (
                "2025-03-12T06:46:10Z",
                AuditKind::PositionClosed,
                json!({"ticket": 42, "symbol": "USDJPY"}),
            ),
            (
                "2025-03-11T21:00:00Z",
                AuditKind::ProposalEvaluated,
                json!({"outcome": "queued", "symbol": "usdjpy", "command_id": OPEN}),
            ),
            (
                "2025-03-11T21:00:05Z",
                AuditKind::CommandCompleted,
                json!({"kind": "open_order", "command_id": OPEN, "result": {"ticket": 42}}),
            ),
            (
                "2025-03-12T02:00:00Z",
                AuditKind::ProposalEvaluated,
                json!({"outcome": "held", "symbol": "USDJPY", "ticket": "42"}),
            ),
            (
                "2025-03-12T06:40:00Z",
                AuditKind::ProposalEvaluated,
                json!({"outcome": "close_queued", "symbol": "USDJPY", "ticket": 42, "command_id": CLOSE}),
            ),
            (
                "2025-03-12T06:41:00Z",
                AuditKind::ProposalEvaluated,
                json!({"outcome": "rejected", "symbol": "EURUSD"}),
            ),
            (
                "2025-03-12T06:42:00Z",
                AuditKind::BrokerSnapshot,
                json!({"orders": 0}),
            ),
        ] {
            trail.record_at(instant(at), AuditEvent::new(kind, payload));
        }
    }

    #[test]
    fn audit_kinds_round_trip_their_wire_names() {
        for kind in AuditKind::ALL {
            assert_eq!(AuditKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(AuditKind::parse("order_placed"), None);
    }

    #[test]
    fn query_filters_are_validated_before_storage() {
        let kinds = [AuditKind::ProposalEvaluated];
        assert!(AuditQuery::new(&[], 10).is_err());
        assert!(AuditQuery::new(&kinds, 0).is_err());
        assert!(AuditQuery::new(&kinds, MAX_QUERY_ROWS + 1).is_err());
        let base = AuditQuery::new(
            &[AuditKind::ProposalEvaluated, AuditKind::ProposalEvaluated],
            MAX_QUERY_ROWS,
        )
        .expect("valid");
        assert_eq!(base.kinds(), &kinds, "duplicate kinds collapse");
        assert_eq!(base.limit(), 200);
        assert!(base.clone().with_symbol("bad symbol").is_err());
        assert!(base.clone().with_ticket(0).is_err());
        assert!(base.clone().with_command_ids(&[]).is_err());
        assert!(base.clone().with_command_ids(&["nope".to_owned()]).is_err());
        let too_many: Vec<String> = (0..=MAX_QUERY_COMMAND_IDS)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        assert!(base.clone().with_command_ids(&too_many).is_err());
        assert!(base.clone().with_outcome("").is_err());
        assert!(base.clone().with_outcome("Held").is_err());
        assert!(base.clone().with_outcome(&"x".repeat(65)).is_err());
        assert!(base.clone().with_window(Some(-1), None).is_err());
        assert!(base.clone().with_window(None, Some(-1)).is_err());
        let error = base
            .clone()
            .with_window(Some(5), Some(5))
            .expect_err("empty window");
        assert_eq!(
            error.to_string(),
            "invalid audit query `window`: since must be before until"
        );

        let full = base
            .with_symbol(" usdjpy ")
            .and_then(|query| query.with_ticket(42))
            .and_then(|query| query.with_command_ids(&[OPEN.to_uppercase(), OPEN.to_owned()]))
            .and_then(|query| query.with_outcome("held"))
            .and_then(|query| query.with_window(Some(1), Some(2)))
            .expect("every filter");
        assert_eq!(full.symbol(), Some("USDJPY"));
        assert_eq!(full.ticket(), Some(42));
        assert_eq!(
            full.command_ids(),
            &[OPEN.to_owned()],
            "canonical, deduplicated"
        );
        assert_eq!(full.outcome(), Some("held"));
        assert_eq!((full.since_ms(), full.until_ms()), (Some(1), Some(2)));
    }

    #[test]
    fn matches_defines_every_filter() {
        let kinds = [AuditKind::ProposalEvaluated, AuditKind::CommandCompleted];
        let query = |configure: fn(AuditQuery) -> Result<AuditQuery, AuditQueryError>| {
            configure(AuditQuery::new(&kinds, 10).expect("base")).expect("filter")
        };
        let held = json!({"outcome": "held", "symbol": "usdJPY", "ticket": 42, "command_id": OPEN});
        let fill = json!({"result": {"ticket": "42"}, "command_id": CLOSE});
        let any = query(Ok);
        assert!(any.matches(None, "proposal_evaluated", &held));
        assert!(!any.matches(None, "broker_snapshot", &held), "kind filter");

        let symbol = query(|query| query.with_symbol("USDJPY"));
        assert!(symbol.matches(None, "proposal_evaluated", &held));
        assert!(!symbol.matches(None, "proposal_evaluated", &fill));

        let ticket = query(|query| query.with_ticket(42));
        assert!(ticket.matches(None, "proposal_evaluated", &held));
        assert!(
            ticket.matches(None, "command_completed", &fill),
            "result.ticket as text"
        );
        assert!(!ticket.matches(None, "proposal_evaluated", &json!({"ticket": 7})));

        let ids = query(|query| query.with_command_ids(&[CLOSE.to_owned()]));
        assert!(ids.matches(None, "command_completed", &fill));
        assert!(!ids.matches(None, "proposal_evaluated", &held));

        let outcome = query(|query| query.with_outcome("held"));
        assert!(outcome.matches(None, "proposal_evaluated", &held));
        assert!(!outcome.matches(None, "command_completed", &fill));

        let window = query(|query| query.with_window(Some(100), Some(200)));
        assert!(
            window.matches(Some(100), "proposal_evaluated", &held),
            "since is inclusive"
        );
        assert!(
            !window.matches(Some(200), "proposal_evaluated", &held),
            "until is exclusive"
        );
        assert!(!window.matches(Some(99), "proposal_evaluated", &held));
        assert!(
            !window.matches(None, "proposal_evaluated", &held),
            "unknown times never match a window"
        );
        let open_ended = query(|query| query.with_window(Some(100), None));
        assert!(open_ended.matches(Some(i64::MAX), "proposal_evaluated", &held));
    }

    #[test]
    fn trail_times_round_trip_rfc3339_and_postgres_text() {
        let at = instant("2025-03-12T06:46:09.120Z");
        assert_eq!(format_trail_time(at), "2025-03-12T06:46:09.120Z");
        let expected = Some(ms("2025-03-12T06:46:09.120Z"));
        assert_eq!(parse_trail_time("2025-03-12T06:46:09.120Z"), expected);
        assert_eq!(parse_trail_time("2025-03-12 06:46:09.12+00"), expected);
        assert_eq!(parse_trail_time("2025-03-12 08:46:09.12+02"), expected);
        assert_eq!(parse_trail_time("2025-03-12 12:16:09.12+05:30"), expected);
        assert_eq!(parse_trail_time("2025-03-12 03:46:09.12-03"), expected);
        assert_eq!(parse_trail_time("now"), None);
        assert_eq!(parse_trail_time(""), None);
    }

    #[actix_web::test]
    async fn memory_query_orders_filters_and_limits_like_postgres() {
        let trail = MemoryTrail::default();
        lifecycle(&trail);
        let decisions = [AuditKind::ProposalEvaluated, AuditKind::PositionClosed];
        let rows = trail
            .query(&AuditQuery::new(&decisions, 10).expect("query"))
            .await
            .expect("rows");
        assert_eq!(
            rows.iter().map(|row| row.at.as_str()).collect::<Vec<_>>(),
            vec![
                "2025-03-12T06:46:10.000Z",
                "2025-03-12T06:41:00.000Z",
                "2025-03-12T06:40:00.000Z",
                "2025-03-12T02:00:00.000Z",
                "2025-03-11T21:00:00.000Z",
            ],
            "newest first by record time, not insertion"
        );
        assert!(rows.iter().all(|row| row.id.starts_with("memory-")));

        let limited = trail
            .query(&AuditQuery::new(&decisions, 2).expect("query"))
            .await
            .expect("rows");
        assert_eq!(limited.len(), 2);

        let ticket = AuditQuery::new(
            &[
                AuditKind::ProposalEvaluated,
                AuditKind::PositionClosed,
                AuditKind::CommandCompleted,
            ],
            10,
        )
        .and_then(|query| query.with_ticket(42))
        .expect("query");
        let rows = trail.query(&ticket).await.expect("rows");
        assert_eq!(rows.len(), 4, "ticket as number, as text, and in result");

        let symbol = AuditQuery::new(&decisions, 10)
            .and_then(|query| query.with_symbol("USDJPY"))
            .expect("query");
        assert_eq!(trail.query(&symbol).await.expect("rows").len(), 4);

        let window = AuditQuery::new(&decisions, 10)
            .and_then(|query| {
                query.with_window(
                    Some(ms("2025-03-12T00:00:00Z")),
                    Some(ms("2025-03-12T06:41:00Z")),
                )
            })
            .expect("query");
        let rows = trail.query(&window).await.expect("rows");
        assert_eq!(
            rows.iter()
                .map(|row| row.payload["outcome"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["close_queued", "held"]
        );

        let linked = AuditQuery::new(
            &[AuditKind::ProposalEvaluated, AuditKind::CommandCompleted],
            10,
        )
        .and_then(|query| query.with_command_ids(&[OPEN.to_owned()]))
        .expect("query");
        assert_eq!(trail.query(&linked).await.expect("rows").len(), 2);

        let none = AuditQuery::new(&decisions, 10)
            .and_then(|query| query.with_outcome("break_even"))
            .expect("query");
        assert!(trail.query(&none).await.expect("rows").is_empty());

        let recent = trail.recent(2).await.expect("recent");
        assert_eq!(
            recent[0].kind, "position_closed",
            "recent is newest first too"
        );
        assert_eq!(recent[1].kind, "broker_snapshot");
    }

    /// A store that only knows `recent`, returning PostgreSQL-formatted times,
    /// to prove the default query applies the same semantics.
    #[derive(Debug, Default)]
    struct TextTrail {
        inner: MemoryTrail,
    }

    #[async_trait]
    impl AuditTrail for TextTrail {
        fn provider(&self) -> AuditProvider {
            AuditProvider::Postgres
        }

        async fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
            self.inner.record(event).await
        }

        async fn recent(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError> {
            let mut rows = self.inner.recent(limit).await?;
            for row in &mut rows {
                // `2025-03-12T06:46:10.000Z` -> `2025-03-12 08:46:10+02`.
                let at_ms = parse_trail_time(&row.at).expect("memory time");
                let local =
                    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
                        .expect("time")
                        .to_offset(UtcOffset::from_hms(2, 0, 0).expect("offset"));
                row.at = format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}+02",
                    local.year(),
                    u8::from(local.month()),
                    local.day(),
                    local.hour(),
                    local.minute(),
                    local.second()
                );
            }
            rows.push(AuditRow {
                id: "garbled".to_owned(),
                at: "yesterday-ish".to_owned(),
                kind: "proposal_evaluated".to_owned(),
                payload: json!({"outcome": "held", "ticket": 43}),
            });
            Ok(rows)
        }

        async fn prune(&self, _keep_days: u32) -> Result<u64, AuditError> {
            Ok(0)
        }
    }

    #[actix_web::test]
    async fn default_query_matches_the_memory_semantics() {
        let text = TextTrail::default();
        lifecycle(&text.inner);
        let memory = MemoryTrail::default();
        lifecycle(&memory);
        let queries = [
            AuditQuery::new(
                &[AuditKind::ProposalEvaluated, AuditKind::PositionClosed],
                10,
            )
            .and_then(|query| query.with_ticket(42)),
            AuditQuery::new(&[AuditKind::ProposalEvaluated], 10).and_then(|query| {
                query.with_window(
                    Some(ms("2025-03-12T00:00:00Z")),
                    Some(ms("2025-03-12T06:41:00Z")),
                )
            }),
            AuditQuery::new(&[AuditKind::ProposalEvaluated], 1),
        ];
        for query in queries {
            let query = query.expect("query");
            let from_text = text.query(&query).await.expect("default query");
            let from_memory = memory.query(&query).await.expect("memory query");
            assert_eq!(
                from_text.iter().map(|row| &row.payload).collect::<Vec<_>>(),
                from_memory
                    .iter()
                    .map(|row| &row.payload)
                    .collect::<Vec<_>>(),
                "{query:?}"
            );
        }
        // Without a window the unparseable row is still a candidate.
        let unbounded = AuditQuery::new(&[AuditKind::ProposalEvaluated], 10)
            .and_then(|query| query.with_outcome("held"))
            .expect("query");
        assert_eq!(text.query(&unbounded).await.expect("rows").len(), 2);
        assert!(BrokenTrail.query(&unbounded).await.is_err());
        assert_eq!(text.provider(), AuditProvider::Postgres);
        assert_eq!(text.prune(1).await.expect("prune"), 0);
        text.record(AuditEvent::new(AuditKind::ServiceStarted, json!({})))
            .await
            .expect("record");
        assert_eq!(text.inner.len(), 8);
    }

    #[actix_web::test]
    async fn live_feed_eviction_preserves_cursor_order_and_durable_events() {
        let trail = Arc::new(MemoryTrail::default());
        let runtime = AuditRuntime::new(trail.clone());
        let total = FEED_CAPACITY + 3;
        for marker in 1..=total {
            runtime
                .try_record(AuditEvent::new(
                    AuditKind::BalanceObserved,
                    json!({"marker": marker}),
                ))
                .await;
        }
        assert_eq!(runtime.feed_latest(), total as u64);
        assert_eq!(trail.len(), total, "feed eviction must not prune storage");
        let retained = runtime.feed_after(0, total, Duration::ZERO).await;
        assert_eq!(retained.len(), FEED_CAPACITY);
        assert_eq!(retained.first().expect("first retained").seq, 4);
        assert_eq!(retained.last().expect("last retained").seq, total as u64);
        assert!(
            retained
                .windows(2)
                .all(|pair| pair[1].seq == pair[0].seq + 1)
        );
        for event in &retained {
            assert_eq!(event.payload["marker"], event.seq);
        }
        let page = runtime.feed_after(0, 2, Duration::ZERO).await;
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![4, 5]
        );
        let next = runtime.feed_after(5, 2, Duration::ZERO).await;
        assert_eq!(
            next.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![6, 7]
        );
        assert!(
            runtime
                .feed_after(total as u64, 2, Duration::ZERO)
                .await
                .is_empty()
        );
    }

    #[actix_web::test]
    async fn quiet_live_feed_returns_after_its_bounded_wait() {
        let runtime = AuditRuntime::new(Arc::new(MemoryTrail::default()));
        let events = actix_web::rt::time::timeout(
            Duration::from_secs(1),
            runtime.feed_after(0, 10, Duration::from_millis(10)),
        )
        .await
        .expect("quiet feed must finish its wait");
        assert!(events.is_empty());
        assert_eq!(runtime.feed_latest(), 0);
    }

    #[actix_web::test]
    async fn failing_storage_never_propagates() {
        let runtime = AuditRuntime::new(Arc::new(BrokenTrail));
        runtime
            .try_record(AuditEvent::new(AuditKind::CommandFailed, json!({})))
            .await;
        assert!(runtime.trail().recent(1).await.is_err());
        assert_eq!(runtime.try_prune(30).await, 0, "failures report zero rows");
    }

    #[actix_web::test]
    async fn pruning_is_best_effort_and_keeps_memory_trails_intact() {
        let trail = Arc::new(MemoryTrail::default());
        let runtime = AuditRuntime::new(trail.clone());
        runtime
            .try_record(AuditEvent::new(AuditKind::ServiceStarted, json!({})))
            .await;
        assert_eq!(runtime.try_prune(30).await, 0);
        assert_eq!(trail.len(), 1);
    }
}
