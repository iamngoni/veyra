//! Economic-calendar integration boundary.
//!
//! Scheduled news moves the instruments Veyra trades, so the autopilot treats
//! it as a first-class input: the model sees the upcoming events for every
//! instrument it may trade, and a deterministic blackout refuses an approved
//! draft when a high-impact event for the instrument's currencies falls inside
//! the configured window. Every source implements [`EventCalendar`] and is
//! selected by configuration (`VEYRA_CALENDAR_PROVIDER`); without one both
//! behaviours are inert and the loop behaves as before.

pub mod forexfactory;
pub mod settings;

pub use settings::CalendarSettings;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

/// Largest event title the contract accepts.
const MAX_TITLE_CHARS: usize = 128;

/// Currency code used by events that apply to every instrument.
pub const ALL_CURRENCIES: &str = "ALL";

/// Supported calendar provider implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarProvider {
    /// The ForexFactory weekly calendar export.
    Forexfactory,
}

impl CalendarProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forexfactory => "forexfactory",
        }
    }

    /// Parses a configuration value; unknown providers are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "forexfactory" => Some(Self::Forexfactory),
            _ => None,
        }
    }
}

impl fmt::Display for CalendarProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How strongly a scheduled event tends to move its currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Impact {
    /// Central-bank decisions, inflation prints, and payrolls.
    High,
    /// Second-tier releases worth the model's attention.
    Medium,
    /// Routine data that rarely moves price.
    Low,
    /// Bank holidays; no session is expected for the currency.
    Holiday,
}

impl Impact {
    /// Short identifier used in model input and audit rows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Holiday => "holiday",
        }
    }

    /// Parses a provider value (case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            "holiday" => Some(Self::Holiday),
            _ => None,
        }
    }
}

/// One scheduled economic event for a currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    title: String,
    currency: String,
    impact: Impact,
    time: i64,
}

impl CalendarEvent {
    /// Builds an event from provider fields.
    ///
    /// # Errors
    /// Returns [`CalendarError::Contract`] when the title is empty, control
    /// characters or oversized, the currency is not a three-letter code (or
    /// `ALL`), or the timestamp is not a positive Unix time.
    pub fn new(
        title: &str,
        currency: &str,
        impact: Impact,
        time: i64,
    ) -> Result<Self, CalendarError> {
        let title = title.trim();
        if title.is_empty()
            || title.chars().count() > MAX_TITLE_CHARS
            || title.chars().any(char::is_control)
        {
            return Err(CalendarError::Contract {
                reason: format!(
                    "event title must be 1-{MAX_TITLE_CHARS} characters without control characters"
                ),
            });
        }
        if !valid_currency(currency) {
            return Err(CalendarError::Contract {
                reason: format!(
                    "event currency `{currency}` must be a three-letter code or {ALL_CURRENCIES}"
                ),
            });
        }
        if time <= 0 {
            return Err(CalendarError::Contract {
                reason: "event time must be a positive Unix timestamp".to_owned(),
            });
        }
        Ok(Self {
            title: title.to_owned(),
            currency: currency.to_owned(),
            impact,
            time,
        })
    }

    /// Headline as the provider published it.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Currency the event belongs to (or [`ALL_CURRENCIES`]).
    pub fn currency(&self) -> &str {
        &self.currency
    }

    /// Impact classification.
    pub fn impact(&self) -> Impact {
        self.impact
    }

    /// Unix seconds (UTC) when the event is scheduled.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Whether the event applies to an instrument named like a six-letter
    /// pair (`XAUUSD` responds through its USD leg). Instruments that are not
    /// pairs, such as index CFDs, need [`CalendarEvent::applies_to_currencies`]
    /// with the currencies their contract reports.
    pub fn applies_to(&self, symbol: &str) -> bool {
        self.applies_to_currencies(&instrument_currencies(symbol, None))
    }

    /// Whether the event applies to an instrument moved by `currencies`:
    /// `ALL` events apply everywhere, otherwise the event's currency must be
    /// one of them.
    pub fn applies_to_currencies(&self, currencies: &[String]) -> bool {
        self.currency == ALL_CURRENCIES
            || currencies
                .iter()
                .any(|currency| self.currency.eq_ignore_ascii_case(currency))
    }
}

/// Currencies whose news moves an instrument. The terminal's reported
/// contract currencies win when present (so `SP500m` responds to USD news);
/// otherwise a six-letter name is read as a pair's two legs. Anything else
/// has no currencies and no news applies to it.
pub fn instrument_currencies(
    symbol: &str,
    spec: Option<&crate::broker::SymbolSpecPayload>,
) -> Vec<String> {
    if let Some(currencies) = spec.map(crate::broker::SymbolSpecPayload::currencies)
        && !currencies.is_empty()
    {
        return currencies;
    }
    let bytes = symbol.as_bytes();
    if bytes.len() != 6 || !bytes.iter().all(u8::is_ascii_alphabetic) {
        return Vec::new();
    }
    vec![
        symbol[..3].to_ascii_uppercase(),
        symbol[3..].to_ascii_uppercase(),
    ]
}

/// Events for one instrument inside the configured blackout window.
///
/// Only high-impact events blackout: they are the prints that gap price
/// through a normal stop. The window is applied in both directions, so an
/// entry seconds before a release is refused as well.
pub fn blackout<'a>(
    events: &'a [CalendarEvent],
    symbol: &str,
    now: i64,
    window_minutes: u64,
) -> Option<&'a CalendarEvent> {
    blackout_for(
        events,
        &instrument_currencies(symbol, None),
        now,
        window_minutes,
    )
}

/// [`blackout`] for an instrument moved by `currencies` (see
/// [`instrument_currencies`]).
pub fn blackout_for<'a>(
    events: &'a [CalendarEvent],
    currencies: &[String],
    now: i64,
    window_minutes: u64,
) -> Option<&'a CalendarEvent> {
    if window_minutes == 0 {
        return None;
    }
    let window_secs = window_minutes.saturating_mul(60);
    events.iter().find(|event| {
        event.impact == Impact::High
            && event.applies_to_currencies(currencies)
            && event.time.abs_diff(now) <= window_secs
    })
}

/// Events for one instrument inside `[now, now + horizon_secs)`, oldest first.
pub fn upcoming<'a>(
    events: &'a [CalendarEvent],
    symbol: &str,
    now: i64,
    horizon_secs: i64,
) -> Vec<&'a CalendarEvent> {
    upcoming_for(
        events,
        &instrument_currencies(symbol, None),
        now,
        horizon_secs,
    )
}

/// [`upcoming`] for an instrument moved by `currencies`.
pub fn upcoming_for<'a>(
    events: &'a [CalendarEvent],
    currencies: &[String],
    now: i64,
    horizon_secs: i64,
) -> Vec<&'a CalendarEvent> {
    let until = now.saturating_add(horizon_secs);
    events
        .iter()
        .filter(|event| {
            event.applies_to_currencies(currencies) && event.time >= now && event.time < until
        })
        .collect()
}

fn valid_currency(value: &str) -> bool {
    value == ALL_CURRENCIES
        || (value.len() == 3
            && value
                .chars()
                .all(|character| character.is_ascii_uppercase()))
}

/// Errors raised while constructing or using a calendar feed.
#[derive(Debug, thiserror::Error)]
pub enum CalendarError {
    /// The provider implementation could not be constructed.
    #[error("calendar feed construction failed: {reason}")]
    Construction {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// The provider could not be reached or timed out.
    #[error("calendar feed unavailable: {reason}")]
    Transport {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// The provider answered with a non-success HTTP status.
    #[error("calendar feed returned HTTP status {status}")]
    Status {
        /// HTTP status code the provider returned.
        status: u16,
    },
    /// A provider response violated the calendar contract.
    #[error("calendar feed contract violation: {reason}")]
    Contract {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Narrow contract every calendar implementation implements.
#[async_trait]
pub trait EventCalendar: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output and logs.
    fn provider(&self) -> CalendarProvider;

    /// Events scheduled in `[from, until)`, oldest first.
    ///
    /// # Errors
    /// Returns [`CalendarError::Transport`] / [`CalendarError::Status`] when
    /// the provider cannot answer and [`CalendarError::Contract`] when its
    /// response is unusable.
    async fn events(&self, from: i64, until: i64) -> Result<Vec<CalendarEvent>, CalendarError>;
}

/// Active calendar integration selected by configuration.
#[derive(Debug, Clone)]
pub struct CalendarRuntime {
    feed: Arc<dyn EventCalendar>,
}

impl CalendarRuntime {
    /// Builds the implementation selected by settings.
    ///
    /// # Errors
    /// Returns [`CalendarError::Construction`] when the selected
    /// implementation cannot be built.
    pub fn from_settings(settings: CalendarSettings) -> Result<Self, CalendarError> {
        match settings {
            CalendarSettings::Forexfactory(forexfactory_settings) => Ok(Self {
                feed: Arc::new(forexfactory::ForexfactoryCalendar::new(
                    forexfactory_settings,
                )?),
            }),
        }
    }

    /// Builds a runtime around an injected feed; used by tests and embedders.
    pub fn from_feed(feed: Arc<dyn EventCalendar>) -> Self {
        Self { feed }
    }

    /// Domain-level calendar contract used by the autopilot.
    pub fn feed(&self) -> Arc<dyn EventCalendar> {
        self.feed.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn event(title: &str, currency: &str, impact: Impact, time: i64) -> CalendarEvent {
        CalendarEvent::new(title, currency, impact, time).expect("valid event")
    }

    #[test]
    fn provider_and_impact_names_round_trip() {
        assert_eq!(CalendarProvider::Forexfactory.as_str(), "forexfactory");
        assert_eq!(
            CalendarProvider::parse(" ForexFactory "),
            Some(CalendarProvider::Forexfactory)
        );
        assert_eq!(CalendarProvider::parse("finnhub"), None);
        assert_eq!(CalendarProvider::Forexfactory.to_string(), "forexfactory");

        for impact in [Impact::High, Impact::Medium, Impact::Low, Impact::Holiday] {
            assert_eq!(Impact::parse(impact.as_str()), Some(impact));
        }
        assert_eq!(Impact::parse("severe"), None);
        assert_eq!(Impact::High.as_str(), "high");
    }

    #[actix_web::test]
    async fn the_settings_factory_builds_the_selected_provider() {
        use crate::calendar::settings::ForexfactorySettings;

        let runtime = CalendarRuntime::from_settings(CalendarSettings::Forexfactory(
            ForexfactorySettings::new(Duration::from_secs(5), Duration::from_secs(900)),
        ))
        .expect("factory builds");
        assert_eq!(
            runtime.feed().provider(),
            CalendarProvider::Forexfactory,
            "the configured provider answers"
        );
    }

    #[test]
    fn events_validate_their_fields() {
        let valid = CalendarEvent::new(" Non-Farm Payrolls ", "usd", Impact::High, 1_758_000_000);
        // Lower-case currencies are rejected: providers must publish codes.
        assert!(valid.is_err());

        for (title, currency, time) in [
            ("", "USD", 1_758_000_000_i64),
            ("   ", "USD", 1_758_000_000),
            ("bad\u{7}", "USD", 1_758_000_000),
            (&"x".repeat(MAX_TITLE_CHARS + 1), "USD", 1_758_000_000),
            ("ok", "US", 1_758_000_000),
            ("ok", "USDD", 1_758_000_000),
            ("ok", "usd", 1_758_000_000),
            ("ok", "USD", 0),
            ("ok", "USD", -5),
        ] {
            assert!(
                CalendarEvent::new(title, currency, Impact::Low, time).is_err(),
                "event must be rejected: {title:?} {currency:?} {time}"
            );
        }

        let all = CalendarEvent::new("Summit", ALL_CURRENCIES, Impact::Low, 1).expect("ALL event");
        assert_eq!(all.currency(), ALL_CURRENCIES);
    }

    #[test]
    fn events_match_their_instrument_currencies() {
        let eur = event("ECB", "EUR", Impact::High, 1_000);
        assert!(eur.applies_to("EURUSD"));
        assert!(eur.applies_to("eurusd"));
        assert!(eur.applies_to("EURJPY"));
        assert!(!eur.applies_to("GBPUSD"));
        assert!(!eur.applies_to("XAUUSD"));
        assert!(
            !eur.applies_to("EURUSD.pro"),
            "non-pair symbols never match"
        );

        let gold = event("Gold fix", "XAU", Impact::Medium, 1_000);
        assert!(gold.applies_to("XAUUSD"));

        let universal = event("Summit", ALL_CURRENCIES, Impact::Low, 1_000);
        assert!(universal.applies_to("EURUSD"));
        assert!(universal.applies_to("ANYTHING"));
    }

    #[test]
    fn blackouts_cover_only_high_impact_windows() {
        let now = 1_758_000_000_i64;
        let events = vec![
            event("NFP", "USD", Impact::High, now - 1_200),
            event("Retail sales", "USD", Impact::Medium, now),
            event("ECB", "EUR", Impact::High, now + 900),
            event("CPI", "JPY", Impact::High, now + 60),
        ];

        let hit = blackout(&events, "EURUSD", now, 30).expect("USD print within the window");
        assert_eq!(hit.title(), "NFP");
        assert!(
            blackout(&events, "EURUSD", now, 10).is_none(),
            "10 minutes misses NFP"
        );
        assert_eq!(
            blackout(&events, "USDJPY", now, 10)
                .expect("the JPY print is inside ten minutes")
                .title(),
            "CPI"
        );
        assert!(
            blackout(&events, "AUDNZD", now, 60).is_none(),
            "AUD and NZD have no events in the set"
        );
        assert!(
            blackout(&events, "EURUSD", now, 0).is_none(),
            "a zero window disables the blackout"
        );
        assert!(
            blackout(&events, "AUDNZD", now, 60).is_none(),
            "AUD and NZD have no events in the set"
        );

        // A medium-impact event alone never blackouts.
        let medium = vec![event("Retail sales", "USD", Impact::Medium, now)];
        assert!(blackout(&medium, "EURUSD", now, 60).is_none());
    }

    #[test]
    fn upcoming_lists_only_forward_events_for_the_instrument() {
        let now = 1_758_000_000_i64;
        let events = vec![
            event("Past", "USD", Impact::High, now - 60),
            event("Soon", "USD", Impact::High, now + 60),
            event("Later", "EUR", Impact::Medium, now + 3_600),
            event("Far", "USD", Impact::Low, now + 90_000),
        ];
        let listed = upcoming(&events, "EURUSD", now, 7_200);
        let titles: Vec<&str> = listed.iter().map(|event| event.title()).collect();
        assert_eq!(titles, vec!["Soon", "Later"]);
        assert!(upcoming(&events, "AUDNZD", now, 7_200).is_empty());
    }

    fn spec_json(extra: serde_json::Value) -> crate::broker::SymbolSpecPayload {
        let mut base = serde_json::json!({
            "symbol": "SP500m", "digits": 1, "point": 0.1, "bid": 6500.0, "ask": 6500.5,
            "spreadPoints": 5, "stopLevelPoints": 0, "freezeLevelPoints": 0,
            "lotMin": 0.01, "lotMax": 50.0, "lotStep": 0.01, "tickValue": 0.01,
            "tickSize": 0.1, "marginRequired": 32.5, "swapLong": -1.0, "swapShort": -0.5,
            "swapType": 0, "tradeAllowed": true
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        serde_json::from_value(base).expect("spec parses")
    }

    #[test]
    fn index_news_comes_from_the_contract_currencies() {
        let old_ea = spec_json(serde_json::json!({}));
        assert!(
            old_ea.currencies().is_empty(),
            "an EA before 1.26 reports none"
        );
        assert!(old_ea.sessions.is_empty());
        assert!(instrument_currencies("SP500m", Some(&old_ea)).is_empty());

        let index = spec_json(serde_json::json!({
            "currencyBase": "SP500m", "currencyProfit": "usd",
            "sessions": [{"day": 1, "from": 3600, "to": 82800}]
        }));
        index.validate().expect("valid");
        assert_eq!(
            index.currencies(),
            vec!["USD".to_owned()],
            "a name is not a currency"
        );
        assert_eq!(
            instrument_currencies("SP500m", Some(&index)),
            vec!["USD".to_owned()]
        );
        assert_eq!(
            instrument_currencies("EURUSD", None),
            vec!["EUR".to_owned(), "USD".to_owned()]
        );

        let now = 1_758_000_000_i64;
        let events = vec![event("NFP", "USD", Impact::High, now + 600)];
        let usd = instrument_currencies("SP500m", Some(&index));
        assert_eq!(
            blackout_for(&events, &usd, now, 30)
                .expect("blocked")
                .title(),
            "NFP"
        );
        assert_eq!(upcoming_for(&events, &usd, now, 3_600).len(), 1);
        assert!(blackout_for(&events, &[], now, 30).is_none());

        let broken = spec_json(serde_json::json!({
            "sessions": [{"day": 7, "from": 0, "to": 100}]
        }));
        assert!(broken.validate().is_err());
        let backwards = spec_json(serde_json::json!({
            "sessions": [{"day": 1, "from": 500, "to": 100}]
        }));
        assert!(backwards.validate().is_err());
    }
}
