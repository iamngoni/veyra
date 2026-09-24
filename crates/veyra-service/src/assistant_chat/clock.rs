//! Clock alignment and time windows for assistant observations.
//!
//! MetaTrader stamps order and position times (`openTime`, `closeTime`,
//! `openedAt`) with the broker server clock, not UTC. The retained account
//! snapshot carries a `serverTime` reading (the terminal host's `TimeLocal()`,
//! which matches the server clock on the supported deployment — an explicit
//! assumption, reported with every conversion) together with the instant
//! Veyra received it. The broker offset is therefore
//! `round((serverTime − received) / 900) × 900` seconds: every real UTC
//! offset is a multiple of 15 minutes, and rounding absorbs transport delay.
//! Offsets beyond ±14 h are refused rather than trusted.
//!
//! The operator's own UTC offset is separate. It only changes presentation
//! and what `today` / `yesterday` mean; with no offset, days are UTC days.
//! Everything here is pure except [`BrokerClock::from_state`], which reads
//! the retained snapshot and never contacts the venue.

use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

use crate::AppState;

/// Granularity real UTC offsets use.
const OFFSET_STEP_SECS: i64 = 900;
/// Largest plausible distance between the broker clock and UTC.
const MAX_BROKER_OFFSET_SECS: i64 = 14 * 3_600;
/// Largest operator offset accepted, in minutes (UTC−14:00 through UTC+14:00).
pub(super) const MAX_OPERATOR_OFFSET_MINUTES: i64 = 14 * 60;
/// Milliseconds in one (offset-fixed) day.
pub(super) const DAY_MS: i64 = 86_400_000;

/// Estimated broker-server clock offset from UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BrokerClock {
    offset_secs: i64,
}

impl BrokerClock {
    /// Estimates the offset from one `serverTime` reading and the UTC instant
    /// the snapshot carrying it was received.
    ///
    /// # Errors
    /// Returns a bounded reason when the reading is missing or the rounded
    /// offset exceeds ±14 hours.
    pub(super) fn estimate(server_time: i64, received_utc_secs: i64) -> Result<Self, String> {
        if server_time <= 0 {
            return Err(
                "broker_clock_unknown: the account snapshot carries no server time".to_owned(),
            );
        }
        let drift = server_time.saturating_sub(received_utc_secs);
        let offset_secs = drift
            .saturating_add(OFFSET_STEP_SECS / 2)
            .div_euclid(OFFSET_STEP_SECS)
            .saturating_mul(OFFSET_STEP_SECS);
        if offset_secs.abs() > MAX_BROKER_OFFSET_SECS {
            return Err(format!(
                "broker_clock_implausible: serverTime differs from UTC by {drift} s, beyond ±14 h"
            ));
        }
        Ok(Self { offset_secs })
    }

    /// Estimates the offset from the retained account snapshot.
    ///
    /// # Errors
    /// Returns a bounded reason when no broker or snapshot is available or
    /// the reading is implausible.
    pub(super) fn from_state(state: &AppState) -> Result<Self, String> {
        let link = state
            .broker()
            .ok_or_else(|| "broker_unavailable".to_owned())?
            .link();
        let snapshot = link.last_account().ok_or_else(|| {
            "broker_clock_unknown: no account snapshot has been received yet".to_owned()
        })?;
        let now = state.now();
        let age_secs = link
            .last_account_age(now)
            .map(|age| i64::try_from(age.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let received = unix_secs(now)?.saturating_sub(age_secs);
        Self::estimate(snapshot.server_time, received)
    }

    /// Offset in seconds; positive when the broker clock runs ahead of UTC.
    pub(super) fn offset_secs(self) -> i64 {
        self.offset_secs
    }

    /// Converts a broker-clock Unix reading to real Unix seconds (UTC).
    pub(super) fn to_utc_secs(self, broker_secs: i64) -> i64 {
        broker_secs.saturating_sub(self.offset_secs)
    }

    /// Evidence block describing the conversion for the model.
    pub(super) fn describe(self) -> Value {
        json!({
            "utc_offset": offset_label(self.offset_secs()),
            "utc_offset_secs": self.offset_secs(),
            "basis": "broker server time from the latest account snapshot vs its receipt time, rounded to 15 minutes; broker times below are converted to UTC"
        })
    }
}

/// Evidence block for a conversion that could not be established.
pub(super) fn unknown_clock(reason: &str) -> Value {
    json!({
        "known": false,
        "reason": reason,
        "note": "times suffixed _broker are the broker server clock, not UTC"
    })
}

/// The operator's presentation offset; UTC unless stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct OperatorOffset {
    minutes: Option<i32>,
}

impl OperatorOffset {
    /// Validates an offset in minutes east of UTC.
    ///
    /// # Errors
    /// Returns a bounded reason outside −840 through 840.
    pub(super) fn new(minutes: i64) -> Result<Self, String> {
        if minutes.abs() > MAX_OPERATOR_OFFSET_MINUTES {
            return Err("utc_offset_minutes must be an integer from -840 through 840".to_owned());
        }
        let minutes =
            i32::try_from(minutes).map_err(|_| "utc_offset_minutes is out of range".to_owned())?;
        Ok(Self {
            minutes: Some(minutes),
        })
    }

    /// Stated offset in minutes, when the operator gave one.
    pub(super) fn minutes(self) -> Option<i32> {
        self.minutes
    }

    fn utc_offset(self) -> UtcOffset {
        self.minutes
            .and_then(|minutes| UtcOffset::from_whole_seconds(minutes.saturating_mul(60)).ok())
            .unwrap_or(UtcOffset::UTC)
    }

    /// Formats `ms` in the operator's offset, only when one was stated.
    pub(super) fn local_text(self, ms: i64) -> Option<String> {
        self.minutes?;
        let at = datetime(ms)?.to_offset(self.utc_offset());
        Some(format!(
            "{}{}",
            civil_text(at),
            offset_label(i64::from(at.offset().whole_seconds()))
        ))
    }
}

/// Which end of a named day a keyword resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Edge {
    /// Inclusive start (`since`).
    Start,
    /// Exclusive end (`until`).
    End,
}

/// Resolves one `since` / `until` argument to Unix milliseconds.
///
/// Accepts RFC 3339, `now`, `today`, and `yesterday`. A keyword names a whole
/// day in the operator's offset: `since` takes its start, `until` its end, so
/// `since=yesterday, until=yesterday` covers exactly yesterday.
///
/// # Errors
/// Returns a bounded reason for any other text.
pub(super) fn resolve_instant(
    name: &str,
    raw: &str,
    edge: Edge,
    now_ms: i64,
    operator: OperatorOffset,
) -> Result<i64, String> {
    let trimmed = raw.trim();
    let days_back = match trimmed.to_ascii_lowercase().as_str() {
        "now" => return Ok(now_ms),
        "today" => 0,
        "yesterday" => 1,
        _ => {
            let parsed = OffsetDateTime::parse(trimmed, &Rfc3339).map_err(|_| {
                format!(
                    "{name} must be RFC 3339 (for example 2026-09-24T00:00:00Z), 'now', 'today', or 'yesterday'"
                )
            })?;
            return i64::try_from(parsed.unix_timestamp_nanos() / 1_000_000)
                .map_err(|_| format!("{name} is out of range"));
        }
    };
    let start = local_day_start_ms(now_ms, operator, days_back)
        .ok_or_else(|| format!("{name} could not be resolved against the current clock"))?;
    Ok(match edge {
        Edge::Start => start,
        Edge::End => start.saturating_add(DAY_MS),
    })
}

/// Start of the operator's local day `days_back` days before today, in Unix
/// milliseconds. Offsets are fixed, so every day is exactly 24 hours.
fn local_day_start_ms(now_ms: i64, operator: OperatorOffset, days_back: i64) -> Option<i64> {
    let offset = operator.utc_offset();
    let local = datetime(now_ms)?.to_offset(offset);
    let date: Date = local.date().checked_sub(time::Duration::days(days_back))?;
    let start = date.midnight().assume_offset(offset);
    start.unix_timestamp().checked_mul(1_000)
}

/// Current Unix seconds of a wall-clock instant.
///
/// # Errors
/// Returns a bounded reason when the clock precedes the epoch.
pub(super) fn unix_secs(at: std::time::SystemTime) -> Result<i64, String> {
    let elapsed = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "clock_before_epoch".to_owned())?;
    i64::try_from(elapsed.as_secs()).map_err(|_| "clock_out_of_range".to_owned())
}

/// Current Unix milliseconds of a wall-clock instant.
///
/// # Errors
/// Returns a bounded reason when the clock precedes the epoch.
pub(super) fn unix_ms(at: std::time::SystemTime) -> Result<i64, String> {
    let elapsed = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "clock_before_epoch".to_owned())?;
    i64::try_from(elapsed.as_millis()).map_err(|_| "clock_out_of_range".to_owned())
}

fn datetime(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

fn civil_text(at: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// RFC 3339 UTC text at second precision, or `None` when unrepresentable.
pub(super) fn utc_text(ms: i64) -> Option<String> {
    datetime(ms).map(|at| format!("{}Z", civil_text(at)))
}

/// Broker-clock reading as naive civil text (no zone: it is not UTC).
pub(super) fn broker_text(broker_secs: i64) -> Option<String> {
    datetime(broker_secs.saturating_mul(1_000)).map(civil_text)
}

/// `+HH:MM` / `-HH:MM` label for an offset in seconds.
fn offset_label(offset_secs: i64) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let minutes = offset_secs.abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Compact human duration such as `2h 16m` or `3d 4h`.
pub(super) fn duration_text(secs: i64) -> String {
    let secs = secs.max(0);
    let (days, hours, minutes, seconds) = (
        secs / 86_400,
        secs % 86_400 / 3_600,
        secs % 3_600 / 60,
        secs % 60,
    );
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, _) => format!("{minutes}m {seconds}s"),
        (0, _, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h {minutes}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-24T06:46:09Z, the audit time of USDJPY ticket 10654130's close.
    const CLOSE_UTC: i64 = 1_790_232_369;
    /// 08:46:06 the same day: what the broker stamped as its close time.
    const CLOSE_BROKER: i64 = 1_790_239_566;

    #[test]
    fn broker_offset_rounds_to_quarter_hours_and_converts_to_utc() {
        // A snapshot received three seconds after the broker read its clock.
        let clock = BrokerClock::estimate(CLOSE_BROKER, CLOSE_UTC).expect("offset");
        assert_eq!(clock.offset_secs(), 7_200);
        assert_eq!(clock.to_utc_secs(CLOSE_BROKER), CLOSE_UTC - 3);
        assert_eq!(
            utc_text(clock.to_utc_secs(CLOSE_BROKER) * 1_000).as_deref(),
            Some("2026-09-24T06:46:06Z")
        );
        assert_eq!(clock.describe()["utc_offset"], "+02:00");
        // Negative, half-hour, and just-below-half-step drifts.
        assert_eq!(
            BrokerClock::estimate(1_000_000 - 18_000 + 200, 1_000_000)
                .expect("west")
                .offset_secs(),
            -18_000
        );
        assert_eq!(
            BrokerClock::estimate(1_000_000 + 19_800 - 30, 1_000_000)
                .expect("india")
                .offset_secs(),
            19_800
        );
        assert_eq!(
            BrokerClock::estimate(1_000_000 + 449, 1_000_000)
                .expect("utc")
                .offset_secs(),
            0
        );
    }

    #[test]
    fn implausible_or_missing_broker_clocks_are_refused() {
        let error = BrokerClock::estimate(1_000_000 + 15 * 3_600, 1_000_000).expect_err("15 h");
        assert!(error.starts_with("broker_clock_implausible"), "{error}");
        assert!(BrokerClock::estimate(1_000_000 + 14 * 3_600, 1_000_000).is_ok());
        assert!(BrokerClock::estimate(1_000_000 - 14 * 3_600, 1_000_000).is_ok());
        let missing = BrokerClock::estimate(0, 1_000_000).expect_err("missing");
        assert!(missing.starts_with("broker_clock_unknown"), "{missing}");
        let unknown = unknown_clock("no snapshot");
        assert_eq!(unknown["known"], false);
    }

    #[test]
    fn operator_offsets_are_bounded_and_format_local_times() {
        assert!(OperatorOffset::new(841).is_err());
        assert!(OperatorOffset::new(-841).is_err());
        let harare = OperatorOffset::new(120).expect("offset");
        assert_eq!(harare.minutes(), Some(120));
        assert_eq!(
            harare.local_text(CLOSE_UTC * 1_000).as_deref(),
            Some("2026-09-24T08:46:09+02:00")
        );
        let newfoundland = OperatorOffset::new(-150).expect("offset");
        assert_eq!(
            newfoundland.local_text(CLOSE_UTC * 1_000).as_deref(),
            Some("2026-09-24T04:16:09-02:30")
        );
        assert_eq!(
            OperatorOffset::default().local_text(CLOSE_UTC * 1_000),
            None
        );
        assert_eq!(utc_text(i64::MAX), None);
        assert_eq!(
            broker_text(CLOSE_BROKER).as_deref(),
            Some("2026-09-24T08:46:06")
        );
    }

    #[test]
    fn today_and_yesterday_follow_the_operator_day() {
        // 2026-09-24T23:30:00Z: already the 25th in Harare, still the 24th in UTC.
        let now_ms = 1_790_292_600_000;
        let utc = OperatorOffset::default();
        let harare = OperatorOffset::new(120).expect("offset");
        let start = |raw: &str, operator| {
            resolve_instant("since", raw, Edge::Start, now_ms, operator).expect("resolves")
        };
        let end = |raw: &str, operator| {
            resolve_instant("until", raw, Edge::End, now_ms, operator).expect("resolves")
        };
        assert_eq!(
            utc_text(start("today", utc)).as_deref(),
            Some("2026-09-24T00:00:00Z")
        );
        assert_eq!(
            utc_text(end("today", utc)).as_deref(),
            Some("2026-09-25T00:00:00Z")
        );
        assert_eq!(
            utc_text(start("Yesterday", utc)).as_deref(),
            Some("2026-09-23T00:00:00Z")
        );
        assert_eq!(
            utc_text(end("yesterday", utc)).as_deref(),
            Some("2026-09-24T00:00:00Z")
        );
        assert_eq!(
            utc_text(start("today", harare)).as_deref(),
            Some("2026-09-24T22:00:00Z"),
            "Harare's 25th starts at 22:00 UTC on the 24th"
        );
        assert_eq!(
            utc_text(end("today", harare)).as_deref(),
            Some("2026-09-25T22:00:00Z")
        );
        assert_eq!(
            utc_text(start("yesterday", harare)).as_deref(),
            Some("2026-09-23T22:00:00Z")
        );
        assert_eq!(start(" now ", utc), now_ms);
        assert_eq!(
            start("2026-09-24T08:00:00+02:00", utc),
            1_790_229_600_000,
            "explicit offsets are honoured"
        );
        let error = resolve_instant("since", "last week", Edge::Start, now_ms, utc)
            .expect_err("free text is refused");
        assert!(error.contains("RFC 3339"), "{error}");
    }

    #[test]
    fn clocks_and_durations_are_bounded_text() {
        assert_eq!(duration_text(-5), "0s");
        assert_eq!(duration_text(59), "59s");
        assert_eq!(duration_text(61), "1m 1s");
        assert_eq!(duration_text(8_160), "2h 16m");
        assert_eq!(duration_text(273_600), "3d 4h 0m");
        assert!(unix_secs(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)).is_err());
        assert!(unix_ms(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)).is_err());
        assert_eq!(unix_ms(std::time::UNIX_EPOCH).expect("epoch"), 0);
    }
}
