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
/// Minute the standard week opens on Sunday (and the daily rollover pause
/// begins Monday through Thursday), minutes since midnight UTC.
pub const MARKET_OPEN_MINUTE: u32 = 21 * 60;
/// Minute the daily rollover pause ends, minutes since midnight UTC.
pub const MARKET_RESUME_MINUTE: u32 = 22 * 60;
/// Minute the standard week closes on Friday, minutes since midnight UTC.
pub const MARKET_CLOSE_MINUTE: u32 = 21 * 60;

/// Where the standard trading week stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketState {
    /// The week is open and outside the daily rollover pause.
    Open,
    /// The daily rollover pause (21:00-22:00 UTC, Monday through Thursday).
    Rollover,
    /// The weekend: after Friday's close, before Sunday's open.
    Closed,
}

impl MarketState {
    /// Stable identifier for status output and the console.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Rollover => "rollover",
            Self::Closed => "closed",
        }
    }
}

/// The next scheduled change of the trading week.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// The week reopens (or the rollover pause ends).
    Opens,
    /// The week closes for the weekend.
    Closes,
    /// The daily rollover pause begins.
    Pauses,
    /// The daily rollover pause ends.
    Resumes,
}

impl SessionEvent {
    /// Stable identifier for status output and the console.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opens => "opens",
            Self::Closes => "closes",
            Self::Pauses => "pauses",
            Self::Resumes => "resumes",
        }
    }
}

/// The state of the standard FX/metals week at one instant, with the next
/// scheduled change. Times are UTC; brokers can differ by an hour around
/// daylight-saving switches and around holidays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketSession {
    /// Current state.
    pub state: MarketState,
    /// What happens next.
    pub next_event: SessionEvent,
    /// When it happens (Unix seconds).
    pub next_at: i64,
}

/// The standard trading week at `now`: opens Sunday 21:00 UTC, closes Friday
/// 21:00 UTC, pauses daily 21:00-22:00 UTC Monday through Thursday.
pub fn market_session(now: SystemTime) -> Option<MarketSession> {
    let seconds = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let weekday = ((seconds / 86_400 + 4) % 7) as u8;
    let minute = ((seconds % 86_400) / 60) as u32;
    Some(market_session_at(weekday, minute, seconds))
}

fn market_session_at(weekday: u8, minute: u32, seconds: u64) -> MarketSession {
    let day_start = (seconds - seconds % 86_400) as i64;
    let at =
        |day_offset: i64, minute: u32| day_start + day_offset * 86_400 + i64::from(minute) * 60;
    let open = MarketSession {
        state: MarketState::Open,
        next_event: SessionEvent::Pauses,
        next_at: at(1, MARKET_OPEN_MINUTE),
    };
    match weekday {
        // Saturday: closed until Sunday's open.
        6 => MarketSession {
            state: MarketState::Closed,
            next_event: SessionEvent::Opens,
            next_at: at(1, MARKET_OPEN_MINUTE),
        },
        // Sunday: closed before 21:00, then the week opens.
        0 if minute < MARKET_OPEN_MINUTE => MarketSession {
            state: MarketState::Closed,
            next_event: SessionEvent::Opens,
            next_at: at(0, MARKET_OPEN_MINUTE),
        },
        0 => open,
        // Monday through Thursday: open, with the daily pause in the middle,
        // and Thursday night points at Friday's weekend close.
        1..=4 if minute < MARKET_OPEN_MINUTE => MarketSession {
            next_at: at(0, MARKET_OPEN_MINUTE),
            ..open
        },
        1..=4 if minute < MARKET_RESUME_MINUTE => MarketSession {
            state: MarketState::Rollover,
            next_event: SessionEvent::Resumes,
            next_at: at(0, MARKET_RESUME_MINUTE),
        },
        1..=4 if weekday == 4 => MarketSession {
            state: MarketState::Open,
            next_event: SessionEvent::Closes,
            next_at: at(1, MARKET_CLOSE_MINUTE),
        },
        1..=4 => open,
        // Friday: open until 21:00, then closed until Sunday's open.
        _ if minute < MARKET_CLOSE_MINUTE => MarketSession {
            state: MarketState::Open,
            next_event: SessionEvent::Closes,
            next_at: at(0, MARKET_CLOSE_MINUTE),
        },
        _ => MarketSession {
            state: MarketState::Closed,
            next_event: SessionEvent::Opens,
            next_at: at(2, MARKET_OPEN_MINUTE),
        },
    }
}

/// How long before Friday's close the weekend checkpoint window opens. It
/// starts exactly at the Friday entry cutoff (19:00 UTC), so from then until
/// the close the tick manages the book and nothing new is opened.
pub const WEEKEND_PREP_SECS: u64 = 2 * 60 * 60;

/// The final stretch before the weekend close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeekendPrep {
    /// When the week closes, Unix seconds.
    pub closes_at: i64,
    /// Whole seconds remaining until the close.
    pub closes_in_secs: i64,
}

/// The window in which open positions get their weekend verdict: the last two
/// hours before Friday's close, while the market is still open and a staged
/// close can still execute. `None` outside it, including once the market has
/// closed — a position that ran into the weekend belongs to the broker until
/// the Sunday open, so a verdict then would only queue a command nothing can
/// execute.
pub fn weekend_prep(now: SystemTime) -> Option<WeekendPrep> {
    let seconds = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let session = market_session(now)?;
    if session.next_event != SessionEvent::Closes {
        return None;
    }
    let closes_in_secs = session.next_at.checked_sub(seconds as i64)?;
    if closes_in_secs <= 0 || closes_in_secs > WEEKEND_PREP_SECS as i64 {
        return None;
    }
    Some(WeekendPrep {
        closes_at: session.next_at,
        closes_in_secs,
    })
}

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

/// Minutes before an instrument's session closes when entries stop, so a new
/// position is never opened into a daily break or the weekend close.
pub const SESSION_CLOSE_MARGIN_SECS: i64 = 15 * 60;

/// Whether an instrument's own trading session allows an entry, from the
/// sessions its terminal reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstrumentSession {
    /// No sessions reported, or the broker clock is unknown. The standard
    /// FX/metals week still applies through [`entry_block`].
    #[default]
    Unknown,
    /// Inside a session, at least [`SESSION_CLOSE_MARGIN_SECS`] before it closes.
    Open,
    /// Outside every session, or about to close.
    Closed,
}

/// [`instrument_session`] with a fail-closed default: an instrument that is
/// neither FX nor a metal (so the standard FX week does not describe it) and
/// is not weekend-capable must report its sessions, or it counts as closed.
/// Index CFDs therefore need EA 1.26, which reports them.
pub fn entry_session(
    spec: &crate::broker::SymbolSpecPayload,
    symbol: &crate::broker::Symbol,
    weekend_capable: bool,
    utc_secs: i64,
    broker_offset_secs: Option<i64>,
) -> InstrumentSession {
    if spec.sessions.is_empty()
        && !weekend_capable
        && !crate::risk::valuation::supports_static_valuation(symbol)
    {
        return InstrumentSession::Closed;
    }
    instrument_session(&spec.sessions, utc_secs, broker_offset_secs)
}

/// Decides [`InstrumentSession`] for `sessions` (server time) at `utc_secs`,
/// given the broker's offset from UTC. Adjacent sessions (Monday ending at
/// 24:00 and Tuesday starting at 00:00, or Saturday into Sunday) count as one,
/// so the margin only applies before a real close.
pub fn instrument_session(
    sessions: &[crate::broker::TradeSession],
    utc_secs: i64,
    broker_offset_secs: Option<i64>,
) -> InstrumentSession {
    const DAY: i64 = 86_400;
    const WEEK: i64 = 7 * DAY;
    let Some(offset) = broker_offset_secs else {
        return InstrumentSession::Unknown;
    };
    if sessions.is_empty() {
        return InstrumentSession::Unknown;
    }
    let server = utc_secs.saturating_add(offset);
    // 1970-01-01 was a Thursday: day 4 with Sunday = 0.
    let week_second = (server.div_euclid(DAY) + 4).rem_euclid(7) * DAY + server.rem_euclid(DAY);

    let mut spans: Vec<(i64, i64)> = sessions
        .iter()
        .map(|session| {
            let day = i64::from(session.day) * DAY;
            (day + i64::from(session.from), day + i64::from(session.to))
        })
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let Some(&(_, end)) = merged
        .iter()
        .find(|(start, end)| *start <= week_second && week_second < *end)
    else {
        return InstrumentSession::Closed;
    };
    // A session running to the end of Saturday continues into Sunday's first
    // session when that one starts at midnight.
    let end = match merged.first() {
        Some(&(0, first_end)) if end == WEEK => WEEK + first_end,
        _ => end,
    };
    if week_second + SESSION_CLOSE_MARGIN_SECS <= end {
        InstrumentSession::Open
    } else {
        InstrumentSession::Closed
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Wednesday 2026-01-07 12:00 UTC.
    fn wednesday_noon() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_767_787_200)
    }

    /// A UTC timestamp from weekday (`0` = Sunday) and minute of day.
    /// 2026-09-13 is a Sunday.
    fn moment(weekday: u8, minute: u32) -> SystemTime {
        let sunday = 1_789_257_600_u64;
        UNIX_EPOCH
            + Duration::from_secs(sunday + u64::from(weekday) * 86_400 + u64::from(minute) * 60)
    }

    #[test]
    fn the_week_opens_and_closes_on_schedule() {
        // Saturday: closed, next is Sunday's 21:00 open.
        let session = market_session(moment(6, 10 * 60)).expect("session");
        assert_eq!(session.state, MarketState::Closed);
        assert_eq!(session.next_event, SessionEvent::Opens);
        assert_eq!(session.next_at % 86_400, i64::from(MARKET_OPEN_MINUTE) * 60);

        // Sunday before 21:00: closed, opens today.
        let session = market_session(moment(0, 20 * 60)).expect("session");
        assert_eq!(session.state, MarketState::Closed);
        assert_eq!(session.next_at % 86_400, i64::from(MARKET_OPEN_MINUTE) * 60);

        // Sunday after 21:00: open, next pause is Monday's rollover.
        let session = market_session(moment(0, 22 * 60)).expect("session");
        assert_eq!(session.state, MarketState::Open);
        assert_eq!(session.next_event, SessionEvent::Pauses);

        // Friday before 21:00: open, closes today.
        let session = market_session(moment(5, 19 * 60)).expect("session");
        assert_eq!(session.state, MarketState::Open);
        assert_eq!(session.next_event, SessionEvent::Closes);
        assert_eq!(
            session.next_at % 86_400,
            i64::from(MARKET_CLOSE_MINUTE) * 60
        );

        // Friday after 21:00: closed until Sunday.
        let session = market_session(moment(5, 21 * 60 + 30)).expect("session");
        assert_eq!(session.state, MarketState::Closed);
        assert_eq!(session.next_event, SessionEvent::Opens);
        assert_eq!(session.next_at % 86_400, i64::from(MARKET_OPEN_MINUTE) * 60);
    }

    #[test]
    fn the_daily_rollover_pause_is_reported() {
        // Wednesday before the pause: open, pauses at 21:00.
        let session = market_session(moment(3, 20 * 60)).expect("session");
        assert_eq!(session.state, MarketState::Open);
        assert_eq!(session.next_event, SessionEvent::Pauses);
        assert_eq!(session.next_at % 86_400, i64::from(MARKET_OPEN_MINUTE) * 60);

        // Wednesday inside the pause: rollover, resumes at 22:00.
        let session = market_session(moment(3, 21 * 60 + 10)).expect("session");
        assert_eq!(session.state, MarketState::Rollover);
        assert_eq!(session.next_event, SessionEvent::Resumes);
        assert_eq!(
            session.next_at % 86_400,
            i64::from(MARKET_RESUME_MINUTE) * 60
        );

        // Wednesday after the pause: open again, next pause tomorrow.
        let session = market_session(moment(3, 22 * 60 + 30)).expect("session");
        assert_eq!(session.state, MarketState::Open);
        assert_eq!(session.next_event, SessionEvent::Pauses);
        assert!(session.next_at % 86_400 == i64::from(MARKET_OPEN_MINUTE) * 60);

        // Thursday after the pause: the next change is Friday's close.
        let session = market_session(moment(4, 23 * 60)).expect("session");
        assert_eq!(session.state, MarketState::Open);
        assert_eq!(session.next_event, SessionEvent::Closes);

        assert_eq!(MarketState::Open.as_str(), "open");
        assert_eq!(MarketState::Rollover.as_str(), "rollover");
        assert_eq!(MarketState::Closed.as_str(), "closed");
        assert_eq!(SessionEvent::Opens.as_str(), "opens");
        assert_eq!(SessionEvent::Closes.as_str(), "closes");
        assert_eq!(SessionEvent::Pauses.as_str(), "pauses");
        assert_eq!(SessionEvent::Resumes.as_str(), "resumes");
    }

    #[test]
    fn every_window_block_names_and_describes_itself() {
        for block in [
            WindowBlock::SessionClosed,
            WindowBlock::RolloverBlackout,
            WindowBlock::WeekendApproach,
            WindowBlock::WeekendOpen,
        ] {
            assert!(!block.as_str().is_empty());
            assert!(!block.detail().is_empty());
        }
        assert_eq!(WindowBlock::WeekendApproach.as_str(), "weekend_approach");
        assert_eq!(
            WindowBlock::WeekendApproach.detail(),
            "the weekend entry cutoff has passed"
        );
        assert_eq!(WindowBlock::SessionClosed.as_str(), "session_closed");
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

    #[test]
    fn the_weekend_prep_window_brackets_fridays_close() {
        // Friday before the cutoff: too early, the week is still trading.
        assert!(weekend_prep(moment(5, 18 * 60 + 59)).is_none());

        // At the entry cutoff the window opens, two hours before the close.
        let prep = weekend_prep(moment(5, 19 * 60)).expect("prep");
        assert_eq!(prep.closes_in_secs, WEEKEND_PREP_SECS as i64);
        assert_eq!(prep.closes_at % 86_400, i64::from(MARKET_CLOSE_MINUTE) * 60);

        // Inside the window, the countdown is what remains until the close.
        let prep = weekend_prep(moment(5, 20 * 60 + 30)).expect("prep");
        assert_eq!(prep.closes_in_secs, 1800);

        // At and after the close there is nothing left to stage.
        assert!(weekend_prep(moment(5, 21 * 60)).is_none());
        assert!(weekend_prep(moment(5, 23 * 60)).is_none());

        // Thursday night points at the same close but is a day early, and the
        // daily pause points at the resume rather than the close.
        assert!(weekend_prep(moment(4, 22 * 60)).is_none());
        assert!(weekend_prep(moment(3, 20 * 60)).is_none());

        // The weekend itself is closed.
        assert!(weekend_prep(moment(6, 12 * 60)).is_none());
        assert!(weekend_prep(moment(0, 23 * 60)).is_none());
    }

    fn session(day: u8, from_h: u32, to_h: u32) -> crate::broker::TradeSession {
        crate::broker::TradeSession {
            day,
            from: from_h * 3_600,
            to: to_h * 3_600,
        }
    }

    #[test]
    fn instrument_sessions_open_close_and_stop_entries_before_a_close() {
        use super::{InstrumentSession, instrument_session};
        // Wednesday 2026-01-07 12:00 UTC; broker clock UTC+2 -> 14:00 server.
        let wednesday_noon = 1_767_787_200;
        let offset = Some(7_200);
        // US index CFD: 01:00-23:00 server time with a daily break.
        let index: Vec<_> = (1..=5).map(|day| session(day, 1, 23)).collect();
        assert_eq!(
            instrument_session(&index, wednesday_noon, offset),
            InstrumentSession::Open
        );
        // 22:50 server: inside the session but within 15 minutes of its close.
        let late = wednesday_noon + 8 * 3_600 + 50 * 60;
        assert_eq!(
            instrument_session(&index, late, offset),
            InstrumentSession::Closed
        );
        // 23:30 server: in the daily break.
        let break_time = wednesday_noon + 9 * 3_600 + 30 * 60;
        assert_eq!(
            instrument_session(&index, break_time, offset),
            InstrumentSession::Closed
        );
        // Saturday: no session.
        let saturday = wednesday_noon + 3 * 86_400;
        assert_eq!(
            instrument_session(&index, saturday, offset),
            InstrumentSession::Closed
        );
        // Unknown without sessions or without the broker clock.
        assert_eq!(
            instrument_session(&[], wednesday_noon, offset),
            InstrumentSession::Unknown
        );
        assert_eq!(
            instrument_session(&index, wednesday_noon, None),
            InstrumentSession::Unknown
        );
    }

    #[test]
    fn back_to_back_sessions_count_as_one() {
        use super::{InstrumentSession, instrument_session};
        // FX-style: Monday-Thursday all day, Friday until 22:00 server time.
        let mut fx: Vec<_> = (1..=4).map(|day| session(day, 0, 24)).collect();
        fx.push(session(5, 0, 22));
        // Monday 2026-01-05 23:55 (UTC+0): midnight is not a close.
        let monday_late = 1_767_787_200 - 36 * 3_600 - 5 * 60;
        assert_eq!(
            instrument_session(&fx, monday_late, Some(0)),
            InstrumentSession::Open
        );
        // Crypto-style: every day all day, across the week boundary.
        let always: Vec<_> = (0..=6).map(|day| session(day, 0, 24)).collect();
        // Saturday 2026-01-10 23:55 UTC runs on into Sunday.
        let saturday_late = 1_767_787_200 + 3 * 86_400 + 11 * 3_600 + 55 * 60;
        assert_eq!(
            instrument_session(&always, saturday_late, Some(0)),
            InstrumentSession::Open
        );
    }

    #[test]
    fn instruments_outside_the_fx_week_must_report_their_hours() {
        use super::{InstrumentSession, entry_session};
        let spec = |symbol: &str, sessions: Vec<crate::broker::TradeSession>| {
            crate::broker::SymbolSpecPayload {
                currency_base: None,
                currency_profit: None,
                sessions,
                symbol: symbol.to_owned(),
                digits: 1,
                point: 0.1,
                bid: 1.0,
                ask: 1.1,
                spread_points: 1,
                stop_level_points: 0,
                freeze_level_points: 0,
                lot_min: 0.01,
                lot_max: 1.0,
                lot_step: 0.01,
                tick_value: 0.01,
                tick_size: 0.1,
                margin_required: 1.0,
                swap_long: 0.0,
                swap_short: 0.0,
                swap_type: 0,
                trade_allowed: true,
            }
        };
        let wednesday_noon = 1_767_787_200;
        let index = crate::broker::Symbol::parse("SP500m").expect("symbol");
        let fx = crate::broker::Symbol::parse("EURUSD").expect("symbol");
        let crypto = crate::broker::Symbol::parse("BTCUSD").expect("symbol");
        assert_eq!(
            entry_session(
                &spec("SP500m", Vec::new()),
                &index,
                false,
                wednesday_noon,
                Some(0)
            ),
            InstrumentSession::Closed,
            "an index without reported hours fails closed"
        );
        assert_eq!(
            entry_session(
                &spec("EURUSD", Vec::new()),
                &fx,
                false,
                wednesday_noon,
                Some(0)
            ),
            InstrumentSession::Unknown,
            "FX falls back to the standard week"
        );
        assert_eq!(
            entry_session(
                &spec("BTCUSD", Vec::new()),
                &crypto,
                true,
                wednesday_noon,
                Some(0)
            ),
            InstrumentSession::Unknown,
            "weekend-capable instruments keep their own contract checks"
        );
        let hours: Vec<_> = (1..=5).map(|day| session(day, 1, 23)).collect();
        assert_eq!(
            entry_session(
                &spec("SP500m", hours),
                &index,
                false,
                wednesday_noon,
                Some(0)
            ),
            InstrumentSession::Open
        );
    }
}
