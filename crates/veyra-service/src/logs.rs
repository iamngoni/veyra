//! Bounded in-process log buffer powering the console's agent-log panel.
//!
//! The tracing pipeline writes JSON to stderr for launchd; this layer tees the
//! same events into a bounded ring buffer so the loopback console can show
//! what the service is doing without shell access. Only structured tracing
//! metadata is captured — no request bodies, credentials, or account data —
//! and the buffer is process-lifetime, like the audit event feed.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Records kept before the oldest are evicted.
pub const DEFAULT_CAPACITY: usize = 2048;

/// One captured tracing event.
#[derive(Debug, Clone, Serialize)]
pub struct LogRecord {
    /// Monotonic sequence number; cursors are compared against it.
    pub seq: u64,
    /// Wall-clock capture time, milliseconds since the Unix epoch.
    #[serde(rename = "atMs")]
    pub at_ms: u64,
    /// Lowercase level name (`trace`, `debug`, `info`, `warn`, `error`).
    pub level: String,
    /// Module or crate that emitted the event.
    pub target: String,
    /// Rendered `message` field; empty when the event has none.
    pub message: String,
    /// Remaining structured fields.
    pub fields: Map<String, Value>,
}

/// Parses a level name case-insensitively; unknown names return `None`.
pub fn parse_level(value: &str) -> Option<Level> {
    match value.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(Level::TRACE),
        "debug" => Some(Level::DEBUG),
        "info" => Some(Level::INFO),
        "warn" | "warning" => Some(Level::WARN),
        "error" => Some(Level::ERROR),
        _ => None,
    }
}

/// Process-lifetime bounded log store.
#[derive(Debug)]
pub struct LogBuffer {
    capacity: usize,
    latest: AtomicU64,
    records: Mutex<VecDeque<LogRecord>>,
}

impl LogBuffer {
    /// Builds a shared buffer keeping at most `capacity` records.
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.max(1),
            latest: AtomicU64::new(0),
            records: Mutex::new(VecDeque::new()),
        })
    }

    /// Sequence number of the newest captured record (zero when empty).
    pub fn latest(&self) -> u64 {
        self.latest.load(Ordering::Relaxed)
    }

    /// Appends one record, evicting the oldest beyond capacity.
    pub(crate) fn push(
        &self,
        level: impl Into<String>,
        target: String,
        message: String,
        fields: Map<String, Value>,
    ) {
        let seq = self.latest.fetch_add(1, Ordering::Relaxed) + 1;
        let record = LogRecord {
            seq,
            at_ms: now_millis(),
            level: level.into(),
            target,
            message,
            fields,
        };
        let mut guard = match self.records.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push_back(record);
        while guard.len() > self.capacity {
            guard.pop_front();
        }
    }

    /// Returns records at or above `min_level`.
    ///
    /// With `after == 0` the newest `limit` records are returned oldest-first;
    /// with a cursor, only records after it, keeping the tail call cheap.
    pub fn tail(&self, after: u64, limit: usize, min_level: Level) -> Vec<LogRecord> {
        let guard = match self.records.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let threshold = level_rank_of(min_level);
        let matching: Vec<LogRecord> = guard
            .iter()
            .filter(|record| {
                level_rank(&record.level).is_some_and(|rank| rank <= threshold)
                    && record.seq > after
            })
            .cloned()
            .collect();
        if after == 0 {
            let skip = matching.len().saturating_sub(limit);
            matching.into_iter().skip(skip).collect()
        } else {
            matching.into_iter().take(limit).collect()
        }
    }
}

/// Lower rank means more severe; unknown levels are treated as `trace`.
fn level_rank(level: &str) -> Option<u8> {
    match level {
        "error" => Some(0),
        "warn" => Some(1),
        "info" => Some(2),
        "debug" => Some(3),
        "trace" => Some(4),
        _ => None,
    }
}

fn level_rank_of(level: Level) -> u8 {
    match level {
        Level::ERROR => 0,
        Level::WARN => 1,
        Level::INFO => 2,
        Level::DEBUG => 3,
        Level::TRACE => 4,
    }
}

/// Tracing layer that tees every event into a [`LogBuffer`].
#[derive(Debug)]
pub struct LogLayer {
    buffer: Arc<LogBuffer>,
}

impl LogLayer {
    /// Builds a layer writing into `buffer`.
    pub fn new(buffer: Arc<LogBuffer>) -> Self {
        Self { buffer }
    }
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(
            metadata.level().as_str().to_ascii_lowercase(),
            metadata.target().to_owned(),
            visitor.message,
            visitor.fields,
        );
    }
}

/// Collects an event's fields; the `message` field is rendered separately.
#[derive(Debug, Default)]
struct FieldVisitor {
    message: String,
    fields: Map<String, Value>,
}

impl FieldVisitor {
    fn push(&mut self, field: &Field, value: Value) {
        if field.name() == "message" {
            self.message = match value {
                Value::String(text) => text,
                other => other.to_string(),
            };
        } else {
            self.fields.insert(field.name().to_owned(), value);
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, Value::String(value.to_owned()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field, Value::String(format!("{value:?}")));
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn levels_parse_case_insensitively() {
        assert_eq!(parse_level(" WARN "), Some(Level::WARN));
        assert_eq!(parse_level("warning"), Some(Level::WARN));
        assert_eq!(parse_level("ERROR"), Some(Level::ERROR));
        assert_eq!(parse_level("verbose"), None);
    }

    #[test]
    fn tail_returns_newest_first_page_and_filters_levels() {
        let buffer = LogBuffer::new(3);
        buffer.push("info", "a".to_owned(), "one".to_owned(), Map::new());
        buffer.push("warn", "b".to_owned(), "two".to_owned(), Map::new());
        buffer.push("error", "c".to_owned(), "three".to_owned(), Map::new());
        buffer.push("info", "d".to_owned(), "four".to_owned(), Map::new());
        assert_eq!(buffer.latest(), 4);

        let all = buffer.tail(0, 10, Level::TRACE);
        assert_eq!(all.len(), 3, "capacity evicts the oldest");
        assert_eq!(all[0].message, "two");

        let warnings = buffer.tail(0, 10, Level::WARN);
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].level, "warn");

        let after = buffer.tail(2, 10, Level::TRACE);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].seq, 3);
    }

    #[test]
    fn cursor_pages_in_order() {
        let buffer = LogBuffer::new(16);
        for index in 0..5 {
            buffer.push("info", "t".to_owned(), format!("line {index}"), Map::new());
        }
        let page = buffer.tail(1, 2, Level::TRACE);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].message, "line 1");
        assert_eq!(page[1].message, "line 2");
        assert!(buffer.tail(5, 10, Level::TRACE).is_empty());
    }

    #[test]
    fn layer_captures_message_fields_and_level() {
        let buffer = LogBuffer::new(8);
        let subscriber = tracing_subscriber::registry().with(LogLayer::new(buffer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(symbol = "EURUSD", lots = 0.01, "tick rejected");
            tracing::info!("plain message");
        });

        let records = buffer.tail(0, 10, Level::TRACE);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].level, "warn");
        assert_eq!(records[0].message, "tick rejected");
        assert_eq!(records[0].fields["symbol"], json!("EURUSD"));
        assert_eq!(records[0].fields["lots"], json!(0.01));
        assert_eq!(records[1].message, "plain message");
    }
}
