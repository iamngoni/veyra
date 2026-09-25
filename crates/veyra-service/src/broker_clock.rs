//! Broker-clock alignment shared by the assistant's journal tools and the
//! `/trades` reporting route.
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
//! Everything here is pure except [`BrokerClock::from_state`], which reads
//! the retained snapshot and never contacts the venue.

use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::AppState;

/// Granularity real UTC offsets use.
const OFFSET_STEP_SECS: i64 = 900;
/// Largest plausible distance between the broker clock and UTC.
const MAX_BROKER_OFFSET_SECS: i64 = 14 * 3_600;

/// Estimated broker-server clock offset from UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerClock {
    offset_secs: i64,
}

impl BrokerClock {
    /// Estimates the offset from one `serverTime` reading and the UTC instant
    /// the snapshot carrying it was received.
    ///
    /// # Errors
    /// Returns a bounded reason when the reading is missing or the rounded
    /// offset exceeds ±14 hours.
    pub fn estimate(server_time: i64, received_utc_secs: i64) -> Result<Self, String> {
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
    pub fn from_state(state: &AppState) -> Result<Self, String> {
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
    pub fn offset_secs(self) -> i64 {
        self.offset_secs
    }

    /// Converts a broker-clock Unix reading to real Unix seconds (UTC).
    pub fn to_utc_secs(self, broker_secs: i64) -> i64 {
        broker_secs.saturating_sub(self.offset_secs)
    }

    /// Evidence block describing the conversion for the model.
    pub fn describe(self) -> Value {
        json!({
            "utc_offset": offset_label(self.offset_secs()),
            "utc_offset_secs": self.offset_secs(),
            "basis": "broker server time from the latest account snapshot vs its receipt time, rounded to 15 minutes; broker times below are converted to UTC"
        })
    }
}

/// Current Unix seconds of a wall-clock instant.
///
/// # Errors
/// Returns a bounded reason when the clock precedes the epoch.
pub fn unix_secs(at: std::time::SystemTime) -> Result<i64, String> {
    let elapsed = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "clock_before_epoch".to_owned())?;
    i64::try_from(elapsed.as_secs()).map_err(|_| "clock_out_of_range".to_owned())
}

fn datetime(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

/// Naive civil timestamp text (no zone marker), used both for UTC text (with
/// a trailing `Z` appended by the caller) and for broker-clock readings.
pub(crate) fn civil_text(at: OffsetDateTime) -> String {
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
pub fn utc_text(ms: i64) -> Option<String> {
    datetime(ms).map(|at| format!("{}Z", civil_text(at)))
}

/// `+HH:MM` / `-HH:MM` label for an offset in seconds.
pub(crate) fn offset_label(offset_secs: i64) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let minutes = offset_secs.abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
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
    }

    #[test]
    fn unix_secs_rejects_a_clock_before_the_epoch() {
        assert_eq!(unix_secs(std::time::UNIX_EPOCH).expect("epoch"), 0);
        assert!(unix_secs(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)).is_err());
    }

    #[test]
    fn utc_text_is_bounded() {
        assert_eq!(utc_text(i64::MAX), None);
    }
}
