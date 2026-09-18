//! ForexFactory weekly-calendar implementation.
//!
//! The weekly export at `nfs.faireconomy.media` is a plain JSON array with no
//! API key, refreshed continuously by the publisher, and is the pragmatic free
//! source for scheduled news. Responses are parsed strictly: an unknown impact
//! or malformed date fails the fetch closed, so a provider format change stops
//! entries instead of silently dropping a blackout. The parsed week is cached
//! for the configured lifetime so a short autopilot cadence does not hammer a
//! public endpoint.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;

use crate::calendar::settings::ForexfactorySettings;
use crate::calendar::{
    ALL_CURRENCIES, CalendarError, CalendarEvent, CalendarProvider, EventCalendar, Impact,
};

/// Weekly export URL; fixed so the provider cannot be pointed at a hostile
/// host through configuration.
const CALENDAR_URL: &str = "https://nfs.faireconomy.media/ff_calendar_thisweek.json";
/// Largest response body accepted from the publisher.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct Cached {
    fetched_at: Instant,
    events: Vec<CalendarEvent>,
}

/// ForexFactory weekly-calendar feed.
#[derive(Debug)]
pub struct ForexfactoryCalendar {
    client: reqwest::Client,
    url: String,
    cache_ttl: Duration,
    cached: Mutex<Option<Cached>>,
}

impl ForexfactoryCalendar {
    /// Builds the feed against the publisher's weekly export.
    ///
    /// # Errors
    /// Returns [`CalendarError::Construction`] when the HTTP client cannot be
    /// built.
    pub fn new(settings: ForexfactorySettings) -> Result<Self, CalendarError> {
        Self::with_url(settings, CALENDAR_URL.to_owned())
    }

    /// Builds the feed against an explicit URL; tests point this at a local
    /// server, production always uses the fixed publisher URL.
    fn with_url(settings: ForexfactorySettings, url: String) -> Result<Self, CalendarError> {
        let client = reqwest::Client::builder()
            .timeout(settings.timeout())
            .build()
            .map_err(|error| CalendarError::Construction {
                reason: error.to_string(),
            })?;
        Ok(Self {
            client,
            url,
            cache_ttl: settings.cache_ttl(),
            cached: Mutex::new(None),
        })
    }

    /// Returns the cached week, refetching when the cache lifetime expired.
    async fn fetch(&self, from: i64, until: i64) -> Result<Vec<CalendarEvent>, CalendarError> {
        let fresh = {
            // Same poisoning stance as the rest of the service: writers
            // replace the whole value, readers clone, so recovering is safe.
            let guard = match self.cached.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .as_ref()
                .filter(|cached| cached.fetched_at.elapsed() < self.cache_ttl)
                .map(|cached| cached.events.clone())
        };
        let events = match fresh {
            Some(events) => events,
            None => {
                let fetched = self.fetch_remote().await?;
                let mut guard = match self.cached.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *guard = Some(Cached {
                    fetched_at: Instant::now(),
                    events: fetched.clone(),
                });
                fetched
            }
        };
        Ok(events
            .into_iter()
            .filter(|event| event.time() >= from && event.time() < until)
            .collect())
    }

    async fn fetch_remote(&self) -> Result<Vec<CalendarEvent>, CalendarError> {
        let response =
            self.client
                .get(&self.url)
                .send()
                .await
                .map_err(|error| CalendarError::Transport {
                    reason: error.to_string(),
                })?;
        let status = response.status();
        if !status.is_success() {
            return Err(CalendarError::Status {
                status: status.as_u16(),
            });
        }
        if let Some(length) = response.content_length()
            && length > MAX_BODY_BYTES as u64
        {
            return Err(CalendarError::Contract {
                reason: format!("calendar body exceeds {MAX_BODY_BYTES} bytes"),
            });
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| CalendarError::Transport {
                reason: error.to_string(),
            })?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(CalendarError::Contract {
                reason: format!("calendar body exceeds {MAX_BODY_BYTES} bytes"),
            });
        }
        parse_events(&bytes)
    }
}

#[async_trait]
impl EventCalendar for ForexfactoryCalendar {
    fn provider(&self) -> CalendarProvider {
        CalendarProvider::Forexfactory
    }

    async fn events(&self, from: i64, until: i64) -> Result<Vec<CalendarEvent>, CalendarError> {
        self.fetch(from, until).await
    }
}

/// One raw row of the weekly export.
#[derive(Debug, Deserialize)]
struct RawEvent {
    title: String,
    country: String,
    date: String,
    impact: String,
}

/// Parses the weekly export: every row is validated and the result is sorted
/// oldest first.
fn parse_events(bytes: &[u8]) -> Result<Vec<CalendarEvent>, CalendarError> {
    let rows: Vec<RawEvent> =
        serde_json::from_slice(bytes).map_err(|error| CalendarError::Contract {
            reason: format!("invalid calendar JSON: {error}"),
        })?;
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let impact = Impact::parse(&row.impact).ok_or_else(|| CalendarError::Contract {
            reason: format!("unknown event impact `{}`", row.impact),
        })?;
        let currency = if row.country.eq_ignore_ascii_case("all") {
            ALL_CURRENCIES.to_owned()
        } else {
            row.country.to_ascii_uppercase()
        };
        let time = parse_rfc3339(&row.date)?;
        events.push(CalendarEvent::new(&row.title, &currency, impact, time)?);
    }
    events.sort_by_key(CalendarEvent::time);
    Ok(events)
}

/// Parses the export's RFC 3339 timestamp (`2026-09-13T04:15:00-04:00`).
fn parse_rfc3339(raw: &str) -> Result<i64, CalendarError> {
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map(|parsed| parsed.unix_timestamp())
        .map_err(|error| CalendarError::Contract {
            reason: format!("invalid event date `{raw}`: {error}"),
        })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const SAMPLE: &str = r#"[
        {"title":"Non-Farm Employment Change","country":"USD","date":"2026-09-18T08:30:00-04:00","impact":"High","forecast":"","previous":""},
        {"title":"BRICS Summit","country":"All","date":"2026-09-13T04:15:00-04:00","impact":"Low","forecast":"","previous":""}
    ]"#;

    fn settings(cache_ttl: Duration) -> ForexfactorySettings {
        ForexfactorySettings::new(Duration::from_secs(5), cache_ttl)
    }

    /// One-shot HTTP server: counts requests and answers every connection with
    /// the given status line and body.
    fn spawn_http_server(
        requests: Arc<AtomicUsize>,
        status: &'static str,
        body: &'static str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                requests.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "{status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{address}/calendar.json")
    }

    #[test]
    fn weekly_rows_parse_strictly_and_sort_oldest_first() {
        let events = parse_events(SAMPLE.as_bytes()).expect("sample parses");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].currency(), ALL_CURRENCIES);
        assert_eq!(events[0].impact(), Impact::Low);
        assert_eq!(events[1].currency(), "USD");
        assert_eq!(events[1].impact(), Impact::High);
        assert!(events[0].time() < events[1].time());

        // Offset is honoured: 08:30-04:00 is 12:30 UTC.
        assert_eq!(events[1].time() % 86_400, 12 * 3_600 + 30 * 60);
    }

    #[test]
    fn malformed_feeds_fail_closed() {
        let error = parse_events(b"not json").expect_err("invalid JSON is rejected");
        assert!(matches!(error, CalendarError::Contract { .. }));

        let unknown_impact = r#"[{"title":"X","country":"USD","date":"2026-09-18T08:30:00-04:00","impact":"Severe"}]"#;
        let error =
            parse_events(unknown_impact.as_bytes()).expect_err("unknown impacts are rejected");
        assert!(error.to_string().contains("Severe"), "{error}");

        let bad_date = r#"[{"title":"X","country":"USD","date":"tomorrow","impact":"High"}]"#;
        let error = parse_events(bad_date.as_bytes()).expect_err("bad dates are rejected");
        assert!(error.to_string().contains("tomorrow"), "{error}");

        let bad_currency =
            r#"[{"title":"X","country":"US","date":"2026-09-18T08:30:00-04:00","impact":"High"}]"#;
        assert!(parse_events(bad_currency.as_bytes()).is_err());

        assert!(
            parse_events(b"[]")
                .expect("an empty week is valid")
                .is_empty()
        );
    }

    #[actix_web::test]
    async fn feeds_fetch_cache_and_filter_the_week() {
        let requests = Arc::new(AtomicUsize::new(0));
        let url = spawn_http_server(requests.clone(), "HTTP/1.1 200 OK", SAMPLE);
        let calendar =
            ForexfactoryCalendar::with_url(settings(Duration::from_secs(60)), url).expect("feed");

        assert_eq!(calendar.provider(), CalendarProvider::Forexfactory);
        let events = calendar.events(0, i64::MAX).await.expect("events");
        assert_eq!(events.len(), 2);

        let cached = calendar.events(0, i64::MAX).await.expect("cached events");
        assert_eq!(cached, events);
        assert_eq!(requests.load(Ordering::SeqCst), 1, "second read is cached");

        // Window filtering happens on every read, cached or not.
        let only_nfp = calendar
            .events(events[1].time(), i64::MAX)
            .await
            .expect("filtered");
        assert_eq!(only_nfp.len(), 1);
        assert_eq!(only_nfp[0].title(), "Non-Farm Employment Change");
        assert!(
            calendar
                .events(0, events[0].time())
                .await
                .expect("before")
                .is_empty()
        );
    }

    #[actix_web::test]
    async fn expiring_caches_refetch() {
        let requests = Arc::new(AtomicUsize::new(0));
        let url = spawn_http_server(requests.clone(), "HTTP/1.1 200 OK", SAMPLE);
        let calendar = ForexfactoryCalendar::with_url(settings(Duration::ZERO), url).expect("feed");

        calendar.events(0, i64::MAX).await.expect("first");
        calendar.events(0, i64::MAX).await.expect("second");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "expired cache refetches"
        );
    }

    #[actix_web::test]
    async fn http_failures_are_typed() {
        let requests = Arc::new(AtomicUsize::new(0));
        let url = spawn_http_server(requests, "HTTP/1.1 503 Service Unavailable", "");
        let calendar =
            ForexfactoryCalendar::with_url(settings(Duration::from_secs(60)), url).expect("feed");
        let error = calendar
            .events(0, i64::MAX)
            .await
            .expect_err("503 is a failure");
        assert!(
            matches!(error, CalendarError::Status { status: 503 }),
            "{error}"
        );
    }

    #[actix_web::test]
    async fn connection_failures_are_transport_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let calendar = ForexfactoryCalendar::with_url(
            settings(Duration::from_secs(60)),
            format!("http://{address}/calendar.json"),
        )
        .expect("feed");
        let error = calendar
            .events(0, i64::MAX)
            .await
            .expect_err("a closed port cannot answer");
        assert!(
            matches!(error, CalendarError::Transport { .. }),
            "unexpected error: {error}"
        );
    }
}
