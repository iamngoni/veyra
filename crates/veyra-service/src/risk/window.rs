//! Deterministic entry-window guard.
//!
//! Normal trades only survive the calendar: rollover widens spreads and can
//! freeze stops; Friday's close and Sunday's open gap; the configured session
//! window is the operator's own preference. This module turns those facts into
//! one pure decision the gate can enforce and the agent loop can report.
//!
//! Times are UTC. The rollover window covers 20:45-22:15 UTC, which contains
//! both the winter (22:00) and summer (21:00) New York 17:00 rollovers.

use std::time::{SystemTime, UNIX_EPOCH};

use super::SessionWindow;

/// First minute (inclusive) of the rollover blackout, as minutes since
/// midnight UTC.
pub const ROLLOVER_START_MINUTE: u32 = 20 * 60 + 45;
/// First minute (exclusive) after the rollover blackout.
pub const ROLLOVER_END_MINUTE: u32 = 22 * 60 + 15;
/// Friday entry cutoff (inclusive), minutes since midnight UTC.
pub const FRIDAY_CUTOFF_MINUTE: u32 = 19 * 60;
/// Sunday entries resume at this minute (inclusive), minutes since midnight UTC.
pub const SUNDAY_OPEN_MINUTE: u32 = 23 * 60;

/// Why the entry window is closed, if it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowBlock {
    /// The operator's configured session is closed at this hour.
    SessionClosed,
    /// Inside the daily rollover blackout.
    RolloverBlackout,
    /// After Friday's entry cutoff.
    WeekendApproach,
    /// Before Sunday's reopen.
    WeekendOpen,
}

impl WindowBlock {
    /// Stable identifier for logs and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionClosed => "session_closed",
            Self::RolloverBlackout => "rollover_blackout",
            Self::WeekendApproach => "weekend_approach",
            Self::WeekendOpen => "weekend_open",
        }
    }

    /// Non-sensitive operator explanation.
    pub fn detail(self) -> &'static str {
        match self {
            Self::SessionClosed => "the configured session window is closed",
            Self::RolloverBlackout => "the daily rollover blackout is active",
            Self::WeekendApproach => "the weekend entry cutoff has passed",
            Self::WeekendOpen => "the market has not reopened for the week",
        }
    }
}

/// Weekday and minute-of-day for `now` in UTC.
fn utc_parts(now: SystemTime) -> Option<(u8, u32)> {
    // 1970-01-01 was a Thursday (weekday index 4 with Sunday = 0).
    let seconds = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let days = seconds / 86_400;
    let weekday = ((days + 4) % 7) as u8;
    let minute = ((seconds % 86_400) / 60) as u32;
    Some((weekday, minute))
}

/// Whether `now` is inside the rollover blackout.
pub fn in_rollover(minute: u32) -> bool {
    (ROLLOVER_START_MINUTE..ROLLOVER_END_MINUTE).contains(&minute)
}

/// Why entries are blocked at `now`, combining the configured session with the
/// built-in rollover and weekend guards. `None` means the window is open.
pub fn entry_block(now: SystemTime, session: Option<SessionWindow>) -> Option<WindowBlock> {
    let (weekday, minute) = utc_parts(now)?;
    // Sunday = 0, Friday = 5, Saturday = 6.
    if weekday == 6 {
        return Some(WindowBlock::WeekendOpen);
    }
    if weekday == 5 && minute >= FRIDAY_CUTOFF_MINUTE {
        return Some(WindowBlock::WeekendApproach);
    }
    if weekday == 0 && minute < SUNDAY_OPEN_MINUTE {
        return Some(WindowBlock::WeekendOpen);
    }
    if in_rollover(minute) {
        return Some(WindowBlock::RolloverBlackout);
    }
    if let Some(window) = session {
        let hour = (minute / 60) as u8;
        if !window.contains(hour) {
            return Some(WindowBlock::SessionClosed);
        }
    }
    None
}

/// Human-readable UTC timestamp parts for the agent tool.
pub fn utc_now_parts(now: SystemTime) -> Option<(u8, u32)> {
    utc_parts(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Wednesday 2026-01-07 12:00 UTC.
    fn wednesday_noon() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_767_787_200)
    }

    #[test]
    fn weekday_midday_is_open() {
        assert_eq!(entry_block(wednesday_noon(), None), None);
    }

    #[test]
    fn rollover_blackout_covers_both_dst_vintages() {
        assert!(in_rollover(21 * 60)); // summer rollover
        assert!(in_rollover(22 * 60)); // winter rollover
        assert!(!in_rollover(20 * 60));
        assert!(!in_rollover(23 * 60));
        let summer_rollover = wednesday_noon() + Duration::from_secs(9 * 3_600);
        assert_eq!(
            entry_block(summer_rollover, None),
            Some(WindowBlock::RolloverBlackout)
        );
    }

    #[test]
    fn weekend_guards_close_friday_evening_and_sunday_morning() {
        // Friday 2026-01-09 20:00 UTC.
        let friday_evening = UNIX_EPOCH + Duration::from_secs(1_767_988_800);
        assert_eq!(
            entry_block(friday_evening, None),
            Some(WindowBlock::WeekendApproach)
        );
        // Sunday 2026-01-11 10:00 UTC.
        let sunday_morning = UNIX_EPOCH + Duration::from_secs(1_768_125_600);
        assert_eq!(
            entry_block(sunday_morning, None),
            Some(WindowBlock::WeekendOpen)
        );
        // Saturday is always closed.
        let saturday = sunday_morning - Duration::from_secs(86_400);
        assert_eq!(entry_block(saturday, None), Some(WindowBlock::WeekendOpen));
    }

    #[test]
    fn configured_session_still_applies_outside_guards() {
        let session = SessionWindow::parse("8-16").expect("session");
        assert_eq!(
            entry_block(wednesday_noon(), Some(session)),
            None,
            "noon is inside 08:00-16:00"
        );
        let evening = wednesday_noon() + Duration::from_secs(8 * 3_600);
        assert_eq!(
            entry_block(evening, Some(session)),
            Some(WindowBlock::SessionClosed)
        );
    }

    #[test]
    fn block_reasons_have_stable_names() {
        assert_eq!(WindowBlock::RolloverBlackout.as_str(), "rollover_blackout");
        assert_eq!(WindowBlock::WeekendOpen.as_str(), "weekend_open");
        assert!(!WindowBlock::SessionClosed.detail().is_empty());
    }
}
