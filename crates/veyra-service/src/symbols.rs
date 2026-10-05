//! Catalogue of every instrument the connected broker offers.
//!
//! The console's autopilot picker needs the full list, grouped by kind of
//! market, and the terminal is the only source: it knows what the account's
//! server lists. The list is pulled from the terminal when it first connects,
//! when the account's server changes, when it is a day old, or when the
//! operator asks; every other read is served from memory, so the picker always
//! has data and never waits on MetaTrader.
//!
//! Boundary: this module only describes instruments. Choosing one for the
//! autopilot changes nothing about what may trade: the risk gate's own symbol
//! allowlist stays authoritative, and each entry reports whether the gate
//! would currently accept it.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use actix_web::web::Data;
use actix_web::{HttpResponse, get, post};
use serde::Serialize;
use serde_json::json;

use crate::AppState;
use crate::broker::{
    BrokerLink, CommandPayload, CommandState, Symbol, SymbolListEntry, SymbolListPayload,
    SymbolListRequest,
};

/// Instruments requested per terminal round trip.
pub const PAGE_SIZE: u32 = SymbolListRequest::MAX_LIMIT;
/// How long one page may take before the pull is abandoned.
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// Page cap: with [`PAGE_SIZE`] this bounds a catalogue at 20 000 instruments.
const MAX_PAGES: usize = 100;
/// A held catalogue older than this is pulled again.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Wait after the first failed pull before the next automatic attempt; each
/// further failure doubles it. A failed command is journaled and shown to the
/// operator, so a pull that cannot succeed (an EA built before this command
/// existed) must not repeat every minute.
const RETRY_BACKOFF: Duration = Duration::from_secs(60);
/// Longest wait between automatic attempts. The operator's "Reload from
/// broker" never waits, so this only bounds how soon an updated EA is noticed.
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Kind of market an instrument belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolCategory {
    /// Currency pairs.
    Forex,
    /// Stock-market indices.
    Indices,
    /// Gold, silver, and other metals.
    Metals,
    /// Oil and gas.
    Energies,
    /// Agricultural and other commodities.
    Commodities,
    /// Cryptocurrencies.
    Crypto,
    /// Single-company shares.
    Stocks,
    /// Bonds and rates.
    Bonds,
    /// Anything the broker's naming does not place.
    Other,
}

impl SymbolCategory {
    /// Every category, in the order the console lists them.
    pub const ALL: [Self; 9] = [
        Self::Forex,
        Self::Indices,
        Self::Metals,
        Self::Energies,
        Self::Commodities,
        Self::Crypto,
        Self::Stocks,
        Self::Bonds,
        Self::Other,
    ];

    /// Stable machine name, identical to the serialized form.
    pub fn id(self) -> &'static str {
        match self {
            Self::Forex => "forex",
            Self::Indices => "indices",
            Self::Metals => "metals",
            Self::Energies => "energies",
            Self::Commodities => "commodities",
            Self::Crypto => "crypto",
            Self::Stocks => "stocks",
            Self::Bonds => "bonds",
            Self::Other => "other",
        }
    }

    /// Human label for the console.
    pub fn label(self) -> &'static str {
        match self {
            Self::Forex => "Forex",
            Self::Indices => "Indices",
            Self::Metals => "Metals",
            Self::Energies => "Energies",
            Self::Commodities => "Commodities",
            Self::Crypto => "Crypto",
            Self::Stocks => "Stocks",
            Self::Bonds => "Bonds",
            Self::Other => "Other",
        }
    }
}

/// Folder-name fragments that identify a category, in priority order: an
/// earlier match wins, so `Stock Indices` is an index, not a stock.
const PATH_KEYWORDS: [(SymbolCategory, &[&str]); 8] = [
    (SymbolCategory::Indices, &["indic", "index"]),
    (SymbolCategory::Crypto, &["crypto", "bitcoin", "coin"]),
    (
        SymbolCategory::Metals,
        &["metal", "gold", "silver", "precious"],
    ),
    (SymbolCategory::Energies, &["energ", "oil", "gas"]),
    (
        SymbolCategory::Commodities,
        &["commod", "agri", "soft", "grain"],
    ),
    (SymbolCategory::Bonds, &["bond", "yield", "treasur"]),
    (
        SymbolCategory::Stocks,
        &["stock", "share", "equit", "nasdaq", "nyse"],
    ),
    (
        SymbolCategory::Forex,
        &["forex", "currenc", "major", "minor", "exotic", "cross"],
    ),
];

/// ISO codes that make a six-letter symbol a currency pair.
const CURRENCIES: [&str; 25] = [
    "USD", "EUR", "GBP", "JPY", "CHF", "AUD", "NZD", "CAD", "SEK", "NOK", "DKK", "PLN", "CZK",
    "HUF", "MXN", "ZAR", "TRY", "SGD", "HKD", "CNH", "CNY", "RUB", "THB", "ILS", "RON",
];

const METAL_CODES: [&str; 4] = ["XAU", "XAG", "XPT", "XPD"];
const CRYPTO_CODES: [&str; 14] = [
    "BTC", "ETH", "LTC", "XRP", "BCH", "ADA", "DOT", "DOGE", "SOL", "BNB", "LINK", "XLM", "EOS",
    "TRX",
];
const INDEX_PREFIXES: [&str; 22] = [
    "US30", "US500", "US100", "US2000", "USTEC", "NAS", "SPX", "DJ", "DAX", "GER", "DE30", "DE40",
    "UK100", "FTSE", "FRA", "CAC", "EU50", "STOXX", "JP225", "HK50", "AUS200", "VIX",
];
const ENERGY_PREFIXES: [&str; 8] = [
    "USOIL", "UKOIL", "BRENT", "WTI", "NGAS", "XNG", "XBR", "XTI",
];

/// Places an instrument in a category.
///
/// The broker's folder path is the primary signal (`Forex\Majors\EURUSD`,
/// `Indices\US30`); the symbol's own name decides only when the path says
/// nothing, because naming conventions differ between brokers.
pub fn classify(path: &str, name: &str) -> SymbolCategory {
    let folders: Vec<String> = path
        .split(['\\', '/'])
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    // The last segment is the symbol itself, not a folder.
    let folders = &folders[..folders.len().saturating_sub(1)];
    for (category, keywords) in PATH_KEYWORDS {
        if folders
            .iter()
            .any(|folder| keywords.iter().any(|keyword| folder.contains(keyword)))
        {
            return category;
        }
    }
    if folders.iter().any(|folder| folder == "fx") {
        return SymbolCategory::Forex;
    }
    classify_by_name(name)
}

/// Name-based fallback: the base of `EURUSD.m` is `EURUSD`.
fn classify_by_name(name: &str) -> SymbolCategory {
    let base: String = name
        .to_ascii_uppercase()
        .split(['.', '#', '_', '+', '-'])
        .next()
        .unwrap_or_default()
        .to_owned();
    if base.len() == 6
        && base.is_char_boundary(3)
        && CURRENCIES.contains(&&base[..3])
        && CURRENCIES.contains(&&base[3..])
    {
        return SymbolCategory::Forex;
    }
    if METAL_CODES.iter().any(|code| base.starts_with(code)) {
        return SymbolCategory::Metals;
    }
    if CRYPTO_CODES.iter().any(|code| base.starts_with(code)) {
        return SymbolCategory::Crypto;
    }
    if ENERGY_PREFIXES
        .iter()
        .any(|prefix| base.starts_with(prefix))
    {
        return SymbolCategory::Energies;
    }
    if INDEX_PREFIXES.iter().any(|prefix| base.starts_with(prefix)) {
        return SymbolCategory::Indices;
    }
    SymbolCategory::Other
}

/// One instrument in the catalogue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogEntry {
    /// Terminal symbol name, exactly as the autopilot must be given it.
    pub name: String,
    /// Broker's description, possibly empty.
    pub description: String,
    /// Broker's folder path, possibly empty.
    pub path: String,
    /// Kind of market.
    pub category: SymbolCategory,
}

/// Turns the terminal's raw listing into catalogue entries.
///
/// Symbols Veyra cannot trade are dropped and counted: a name the symbol
/// parser rejects could never be configured, and a duplicate adds nothing.
pub fn build_entries(raw: Vec<SymbolListEntry>) -> (Vec<CatalogEntry>, u32) {
    let mut seen = HashSet::new();
    let mut skipped = 0_u32;
    let mut entries = Vec::with_capacity(raw.len());
    for item in raw {
        let Ok(symbol) = Symbol::parse(&item.name) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(symbol.as_str().to_ascii_lowercase()) {
            skipped += 1;
            continue;
        }
        entries.push(CatalogEntry {
            category: classify(&item.path, symbol.as_str()),
            name: symbol.as_str().to_owned(),
            description: item.description.trim().to_owned(),
            path: item.path.trim().to_owned(),
        });
    }
    entries.sort_by_key(|entry| entry.name.to_ascii_lowercase());
    (entries, skipped)
}

/// Pulls every page of the terminal's listing through `fetch`, which is given
/// each page's starting offset.
///
/// # Errors
/// Returns the first page failure, an offset mismatch, or a note that the
/// listing never ended within [`MAX_PAGES`].
pub async fn collect_pages<F, Fut>(mut fetch: F) -> Result<Vec<SymbolListEntry>, String>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<SymbolListPayload, String>>,
{
    let mut all = Vec::new();
    let mut offset = 0_u32;
    for _ in 0..MAX_PAGES {
        let page = fetch(offset).await?;
        if page.offset != offset {
            return Err(format!(
                "the terminal answered offset {} for a page requested at {offset}",
                page.offset
            ));
        }
        let next = page.next_offset();
        let total = page.total;
        all.extend(page.symbols);
        // No progress means the terminal has nothing further to give.
        if next <= offset || next >= total {
            return Ok(all);
        }
        offset = next;
    }
    Err("the terminal's instrument list did not end within the page limit".to_owned())
}

/// What the catalogue holds after a successful pull.
#[derive(Debug, Clone)]
pub struct CatalogSnapshot {
    server: String,
    fetched_at: SystemTime,
    entries: Arc<Vec<CatalogEntry>>,
    skipped: u32,
}

impl CatalogSnapshot {
    /// Builds a snapshot taken from `server` at `fetched_at`.
    pub fn new(
        server: &str,
        fetched_at: SystemTime,
        entries: Vec<CatalogEntry>,
        skipped: u32,
    ) -> Self {
        Self {
            server: server.to_owned(),
            fetched_at,
            entries: Arc::new(entries),
            skipped,
        }
    }

    /// Broker server the list came from.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// The instruments, sorted by name.
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// Instruments Veyra cannot use and therefore left out.
    pub fn skipped(&self) -> u32 {
        self.skipped
    }

    /// Instruments per category, in console order, omitting empty ones.
    pub fn category_counts(&self) -> Vec<(SymbolCategory, usize)> {
        SymbolCategory::ALL
            .into_iter()
            .map(|category| {
                let count = self
                    .entries
                    .iter()
                    .filter(|entry| entry.category == category)
                    .count();
                (category, count)
            })
            .filter(|(_, count)| *count > 0)
            .collect()
    }
}

/// Shared holder of the latest catalogue and the rules for refreshing it.
#[derive(Debug, Default)]
pub struct SymbolCatalog {
    snapshot: RwLock<Option<CatalogSnapshot>>,
    failure: Mutex<Option<Failure>>,
    refreshing: AtomicBool,
}

/// The latest failed pull and how many have failed in a row.
#[derive(Debug)]
struct Failure {
    at: SystemTime,
    reason: String,
    streak: u32,
}

impl Failure {
    /// Wait before the next automatic attempt: doubled for each failure in a
    /// row, up to [`MAX_RETRY_BACKOFF`].
    fn wait(&self) -> Duration {
        let doublings = self.streak.saturating_sub(1).min(16);
        RETRY_BACKOFF
            .saturating_mul(1_u32 << doublings)
            .min(MAX_RETRY_BACKOFF)
    }
}

/// Proof that one refresh is running; dropping it allows the next.
#[derive(Debug)]
pub struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SymbolCatalog {
    /// An empty catalogue.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current catalogue, if one has been pulled.
    pub fn snapshot(&self) -> Option<CatalogSnapshot> {
        self.snapshot.read().ok()?.clone()
    }

    /// Replaces the catalogue and clears the last failure.
    pub fn store(&self, snapshot: CatalogSnapshot) {
        if let Ok(mut held) = self.snapshot.write() {
            *held = Some(snapshot);
        }
        if let Ok(mut failure) = self.failure.lock() {
            *failure = None;
        }
    }

    /// Whether a pull is due for an account on `server` at `now`: nothing
    /// held, another broker server, or a day-old list.
    pub fn needs_refresh(&self, server: &str, now: SystemTime) -> bool {
        match self.snapshot() {
            None => true,
            Some(held) => {
                held.server != server
                    || now
                        .duration_since(held.fetched_at)
                        .is_ok_and(|age| age >= MAX_AGE)
            }
        }
    }

    /// Remembers a failed pull so automatic retries back off.
    pub fn record_failure(&self, now: SystemTime, reason: &str) {
        if let Ok(mut failure) = self.failure.lock() {
            let streak = failure
                .as_ref()
                .map_or(1, |previous| previous.streak.saturating_add(1));
            *failure = Some(Failure {
                at: now,
                reason: reason.to_owned(),
                streak,
            });
        }
    }

    /// The last pull failure, if no pull has succeeded since.
    pub fn last_failure(&self) -> Option<String> {
        self.failure
            .lock()
            .ok()?
            .as_ref()
            .map(|failure| failure.reason.clone())
    }

    /// Whether the last automatic attempt failed too recently to repeat.
    pub fn in_backoff(&self, now: SystemTime) -> bool {
        self.failure.lock().ok().is_some_and(|failure| {
            failure.as_ref().is_some_and(|failure| {
                now.duration_since(failure.at)
                    .is_ok_and(|elapsed| elapsed < failure.wait())
            })
        })
    }

    /// Claims the right to run a refresh; `None` while another is running.
    pub fn begin(&self) -> Option<RefreshGuard<'_>> {
        self.refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RefreshGuard(&self.refreshing))
    }
}

/// Asks the terminal for one page and waits for its answer.
pub(crate) async fn fetch_page(
    link: &Arc<dyn BrokerLink>,
    offset: u32,
) -> Result<SymbolListPayload, String> {
    let request = SymbolListRequest::new(offset, PAGE_SIZE).map_err(|error| error.to_string())?;
    let id = link.enqueue_list_symbols(request);
    match link.await_command(id, PAGE_TIMEOUT).await {
        CommandState::Completed {
            payload: CommandPayload::SymbolList(page),
        } => Ok(page),
        CommandState::Completed { .. } => {
            Err("the terminal answered with an unexpected payload".to_owned())
        }
        CommandState::Failed { reason } => Err(reason),
        CommandState::Pending => Err("the terminal did not answer in time".to_owned()),
    }
}

/// Pulls the whole catalogue from the connected terminal and stores it.
///
/// Returns how many instruments were stored.
///
/// # Errors
/// Returns a short reason when there is no terminal, another pull is already
/// running, the terminal fails or answers nothing, or the listing is empty.
pub async fn refresh(state: &AppState) -> Result<usize, String> {
    let Some(broker) = state.broker() else {
        return Err("no broker is configured".to_owned());
    };
    let link = broker.link();
    let report = link.report().await;
    let Some(snapshot) = report.snapshot.filter(|snapshot| snapshot.connected()) else {
        return Err("the terminal is not connected".to_owned());
    };
    let catalog = state.symbols();
    let Some(_guard) = catalog.begin() else {
        return Err("a refresh is already running".to_owned());
    };
    let raw = collect_pages(|offset| {
        let link = Arc::clone(&link);
        async move { fetch_page(&link, offset).await }
    })
    .await?;
    let (entries, skipped) = build_entries(raw);
    if entries.is_empty() {
        return Err("the terminal listed no tradable instruments".to_owned());
    }
    let count = entries.len();
    catalog.store(CatalogSnapshot::new(
        snapshot.server().as_str(),
        state.now(),
        entries,
        skipped,
    ));
    Ok(count)
}

/// One maintenance step: pulls the catalogue when it is due.
///
/// Called on a short timer. It does nothing unless a terminal is connected,
/// the catalogue is missing, stale, or from another server, and the last
/// attempt did not fail moments ago.
pub async fn maintain(state: &AppState) {
    let Some(broker) = state.broker() else {
        return;
    };
    let report = broker.link().report().await;
    let Some(snapshot) = report.snapshot.filter(|snapshot| snapshot.connected()) else {
        return;
    };
    let catalog = state.symbols();
    let now = state.now();
    if !catalog.needs_refresh(snapshot.server().as_str(), now) || catalog.in_backoff(now) {
        return;
    }
    match refresh(state).await {
        Ok(count) => tracing::info!(count, "broker instrument list loaded"),
        Err(reason) => {
            // A refresh already running is not a failure of this attempt.
            if state.symbols().begin().is_some() {
                state.symbols().record_failure(now, &reason);
            }
            tracing::warn!(%reason, "broker instrument list unavailable");
        }
    }
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[get("/symbols")]
/// Returns the broker's instrument catalogue for the console's picker.
///
/// Always answers, so the console can render before the terminal has: `ready`
/// is false until the first pull completes and `reason` says why when a pull
/// failed. Each instrument carries whether the risk gate's allowlist would
/// accept it today.
pub async fn symbol_catalog(state: Data<AppState>) -> HttpResponse {
    let catalog = state.symbols();
    let Some(held) = catalog.snapshot() else {
        return HttpResponse::Ok().json(json!({
            "ready": false,
            "reason": catalog.last_failure().unwrap_or_else(|| "waiting for the terminal".to_owned()),
            "categories": [],
            "symbols": [],
        }));
    };
    let policy = state.risk().policy();
    let symbols: Vec<serde_json::Value> = held
        .entries()
        .iter()
        .map(|entry| {
            let risk_allowed = Symbol::parse(&entry.name)
                .map(|symbol| policy.allows_symbol(&symbol))
                .unwrap_or(false);
            json!({
                "name": entry.name,
                "description": entry.description,
                "path": entry.path,
                "category": entry.category,
                "riskAllowed": risk_allowed,
            })
        })
        .collect();
    let categories: Vec<serde_json::Value> = held
        .category_counts()
        .into_iter()
        .map(|(category, count)| {
            json!({ "id": category.id(), "label": category.label(), "count": count })
        })
        .collect();
    HttpResponse::Ok().json(json!({
        "ready": true,
        "server": held.server(),
        "fetchedAt": unix_secs(held.fetched_at),
        "total": held.entries().len(),
        "skipped": held.skipped(),
        "categories": categories,
        "symbols": symbols,
    }))
}

#[post("/symbols/refresh")]
/// Pulls the catalogue from the terminal again, for example after the account
/// moved to another broker server or the broker listed new instruments.
pub async fn refresh_symbols(state: Data<AppState>) -> HttpResponse {
    match refresh(&state).await {
        Ok(count) => HttpResponse::Ok().json(json!({ "status": "refreshed", "count": count })),
        Err(reason) => HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "refresh_failed", "reason": reason })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, path: &str) -> SymbolListEntry {
        SymbolListEntry {
            name: name.to_owned(),
            description: format!("{name} description"),
            path: path.to_owned(),
        }
    }

    #[test]
    fn folders_decide_the_category_before_names_do() {
        for (path, name, expected) in [
            ("Forex\\Majors\\EURUSD", "EURUSD", SymbolCategory::Forex),
            ("Forex\\Exotics\\USDZAR", "USDZAR", SymbolCategory::Forex),
            ("CFD\\Indices\\US30", "US30", SymbolCategory::Indices),
            // A stock-index folder is an index, never a stock.
            ("CFD\\Stock Indices\\WS30", "WS30", SymbolCategory::Indices),
            ("Metals\\XAUUSD", "XAUUSD", SymbolCategory::Metals),
            ("Crypto\\Majors\\BTCUSD", "BTCUSD", SymbolCategory::Crypto),
            ("Energies\\USOIL", "USOIL", SymbolCategory::Energies),
            (
                "Commodities\\Softs\\COFFEE",
                "COFFEE",
                SymbolCategory::Commodities,
            ),
            ("Stocks\\US\\AAPL", "AAPL", SymbolCategory::Stocks),
            ("Bonds\\EUROBUND", "EUROBUND", SymbolCategory::Bonds),
            ("fx\\EURUSD", "EURUSD", SymbolCategory::Forex),
            ("CFD/Indices/DAX40", "DAX40", SymbolCategory::Indices),
            // The folder wins even where the name looks like something else.
            ("Crypto\\EURUSD", "EURUSD", SymbolCategory::Crypto),
        ] {
            assert_eq!(classify(path, name), expected, "{path}");
        }
    }

    #[test]
    fn names_decide_when_the_path_is_silent() {
        for (name, expected) in [
            ("EURUSD", SymbolCategory::Forex),
            ("EURUSD.m", SymbolCategory::Forex),
            ("GBPJPY#", SymbolCategory::Forex),
            ("XAUUSD", SymbolCategory::Metals),
            ("XAGUSD.pro", SymbolCategory::Metals),
            ("BTCUSD", SymbolCategory::Crypto),
            ("ETHUSD", SymbolCategory::Crypto),
            ("USOIL", SymbolCategory::Energies),
            ("BRENT", SymbolCategory::Energies),
            ("US30.cash", SymbolCategory::Indices),
            ("NAS100", SymbolCategory::Indices),
            ("GER40", SymbolCategory::Indices),
            ("FOOBAR", SymbolCategory::Other),
            ("AAPL", SymbolCategory::Other),
            ("", SymbolCategory::Other),
        ] {
            assert_eq!(classify("", name), expected, "{name}");
        }
        // A bare path with only the symbol segment carries no folder either.
        assert_eq!(classify("EURUSD", "EURUSD"), SymbolCategory::Forex);
        // Six letters that are not two currencies are not a pair.
        assert_eq!(classify("", "ABCDEF"), SymbolCategory::Other);
    }

    #[test]
    fn categories_have_stable_ids_labels_and_order() {
        assert_eq!(SymbolCategory::ALL.len(), 9);
        for category in SymbolCategory::ALL {
            assert!(!category.label().is_empty());
            let wire = serde_json::to_value(category).expect("serializes");
            assert_eq!(wire, category.id(), "the wire form is the id");
        }
        assert_eq!(SymbolCategory::Forex.label(), "Forex");
        assert_eq!(SymbolCategory::Other.id(), "other");
    }

    #[test]
    fn entries_drop_untradable_and_duplicate_symbols_and_sort() {
        let (entries, skipped) = build_entries(vec![
            raw("US30", "Indices\\US30"),
            raw("eurusd", "Forex\\EURUSD"),
            raw("EURUSD", "Forex\\EURUSD"),
            raw("bad  name", ""),
            raw("BAD$CHAR", ""),
            raw("AUDUSD", "Forex\\AUDUSD"),
        ]);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(
            names,
            ["AUDUSD", "eurusd", "US30"],
            "sorted, first spelling kept"
        );
        assert_eq!(skipped, 3, "the duplicate and the two unusable names");
        assert_eq!(entries[2].category, SymbolCategory::Indices);
        assert_eq!(entries[0].description, "AUDUSD description");
    }

    fn page(offset: u32, count: u32, total: u32, next: Option<u32>) -> SymbolListPayload {
        SymbolListPayload {
            total,
            offset,
            next,
            symbols: (offset..offset + count)
                .map(|i| raw(&format!("S{i}"), ""))
                .collect(),
        }
    }

    #[actix_web::test]
    async fn pages_are_followed_until_the_listing_ends() {
        let calls = std::sync::Mutex::new(Vec::new());
        let all = collect_pages(|offset| {
            calls.lock().expect("lock").push(offset);
            let answer = match offset {
                0 => page(0, 200, 450, None),
                200 => page(200, 200, 450, None),
                _ => page(400, 50, 450, None),
            };
            async move { Ok(answer) }
        })
        .await
        .expect("collects");
        assert_eq!(all.len(), 450);
        assert_eq!(*calls.lock().expect("lock"), vec![0, 200, 400]);
    }

    #[actix_web::test]
    async fn the_terminals_scan_position_wins_over_the_entry_count() {
        // The terminal skipped 10 unnamed entries in the first page, so the
        // second page must start where its scan ended, not at 190.
        let calls = std::sync::Mutex::new(Vec::new());
        let all = collect_pages(|offset| {
            calls.lock().expect("lock").push(offset);
            let answer = if offset == 0 {
                page(0, 190, 300, Some(200))
            } else {
                page(200, 100, 300, None)
            };
            async move { Ok(answer) }
        })
        .await
        .expect("collects");
        assert_eq!(all.len(), 290);
        assert_eq!(*calls.lock().expect("lock"), vec![0, 200]);
    }

    #[actix_web::test]
    async fn paging_stops_on_failure_mismatch_stall_and_runaway_lists() {
        let failed =
            collect_pages(|_| async { Err::<SymbolListPayload, _>("boom".to_owned()) }).await;
        assert_eq!(failed.expect_err("fails"), "boom");

        let mismatch = collect_pages(|_| async { Ok(page(50, 1, 100, None)) }).await;
        assert!(mismatch.expect_err("fails").contains("offset"));

        // A terminal that returns nothing and never advances ends the pull
        // with what it has instead of looping.
        let stalled = collect_pages(|offset| async move { Ok(page(offset, 0, 100, None)) })
            .await
            .expect("ends");
        assert!(stalled.is_empty());

        let endless =
            collect_pages(|offset| async move { Ok(page(offset, 1, u32::MAX, Some(offset + 1))) })
                .await;
        assert!(endless.expect_err("fails").contains("page limit"));
    }

    fn snapshot(server: &str, at: SystemTime) -> CatalogSnapshot {
        let (entries, skipped) = build_entries(vec![
            raw("EURUSD", "Forex\\EURUSD"),
            raw("US30", "Indices\\US30"),
            raw("GBPUSD", "Forex\\GBPUSD"),
        ]);
        CatalogSnapshot::new(server, at, entries, skipped)
    }

    #[test]
    fn a_pull_is_due_when_nothing_is_held_the_server_changes_or_a_day_passes() {
        let catalog = SymbolCatalog::new();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(catalog.snapshot().is_none());
        assert!(catalog.needs_refresh("Demo", t0), "nothing held");

        catalog.store(snapshot("Demo", t0));
        assert!(!catalog.needs_refresh("Demo", t0 + Duration::from_secs(3600)));
        assert!(catalog.needs_refresh("Real", t0), "another broker server");
        assert!(
            catalog.needs_refresh("Demo", t0 + MAX_AGE),
            "a day-old list is pulled again"
        );
        // A clock that moved backwards must not trigger endless pulls.
        assert!(!catalog.needs_refresh("Demo", t0 - Duration::from_secs(10)));
    }

    #[test]
    fn counts_group_by_category_in_console_order_without_empty_groups() {
        let held = snapshot("Demo", UNIX_EPOCH);
        assert_eq!(
            held.category_counts(),
            vec![(SymbolCategory::Forex, 2), (SymbolCategory::Indices, 1)]
        );
        assert_eq!(held.server(), "Demo");
        assert_eq!(held.entries().len(), 3);
        assert_eq!(held.skipped(), 0);
    }

    #[test]
    fn failures_back_off_and_a_success_clears_them() {
        let catalog = SymbolCatalog::new();
        let t0 = UNIX_EPOCH + Duration::from_secs(5_000);
        assert!(!catalog.in_backoff(t0));
        assert!(catalog.last_failure().is_none());

        catalog.record_failure(t0, "terminal silent");
        assert!(catalog.in_backoff(t0 + Duration::from_secs(10)));
        assert!(!catalog.in_backoff(t0 + RETRY_BACKOFF));
        assert_eq!(catalog.last_failure().as_deref(), Some("terminal silent"));

        catalog.store(snapshot("Demo", t0));
        assert!(!catalog.in_backoff(t0 + Duration::from_secs(10)));
        assert!(catalog.last_failure().is_none());
    }

    use crate::broker::{AccountLogin, AccountSnapshot, BrokerRuntime, BrokerSettings, ServerName};
    use crate::config::{ConfigError, ServiceConfig};
    use crate::risk::{RiskGate, RiskPolicy};
    use serde_json::Value;

    fn config() -> ServiceConfig {
        ServiceConfig::from_source(|name| match name {
            "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
            "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
            "VEYRA_ENV" => Ok("development".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("config must parse")
    }

    fn runtime() -> BrokerRuntime {
        let settings = BrokerSettings::from_source(|name| match name {
            "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
            "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings must parse")
        .expect("configured");
        BrokerRuntime::from_settings(settings).expect("runtime builds")
    }

    fn connect(runtime: &BrokerRuntime, server: &str, connected: bool) {
        runtime
            .ea_link()
            .expect("ea link")
            .record(AccountSnapshot::new(
                AccountLogin::parse(94168).expect("login"),
                ServerName::parse(server).expect("server"),
                Symbol::parse("EURUSD").expect("symbol"),
                connected,
                true,
                0,
                0.0,
            ));
    }

    fn state_with(runtime: Option<BrokerRuntime>) -> AppState {
        AppState::new(
            config(),
            runtime,
            None,
            RiskGate::new(RiskPolicy::default()),
        )
    }

    /// Runs `work` while a fake terminal answers on its own task.
    async fn with_terminal<T>(
        work: impl Future<Output = T>,
        terminal: impl Future<Output = ()> + 'static,
    ) -> T {
        let terminal = actix_web::rt::spawn(terminal);
        let result = work.await;
        terminal.await.expect("the terminal task finished");
        result
    }

    /// Answers every queued page like a terminal listing `total` instruments.
    async fn serve_pages(link: Arc<crate::broker::EaLink>, total: u32, rounds: usize) {
        for _ in 0..rounds {
            let mut answered = false;
            while !answered {
                answered = link.answer_next_symbol_page(|offset, limit| {
                    let end = (offset + limit).min(total);
                    Ok(json!({
                        "total": total,
                        "offset": offset,
                        "symbols": (offset..end)
                            .map(|i| json!({
                                "name": if i == 0 { "EURUSD".to_owned() } else { format!("SYM{i}") },
                                "description": "test",
                                "path": if i == 0 { "Forex\\Majors\\EURUSD" } else { "Indices\\Test" }
                            }))
                            .collect::<Vec<_>>()
                    }))
                });
                if !answered {
                    actix_web::rt::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
    }

    #[actix_web::test]
    async fn refresh_needs_a_broker_and_a_connected_terminal() {
        let none = state_with(None);
        assert_eq!(
            refresh(&none).await.expect_err("no broker"),
            "no broker is configured"
        );
        maintain(&none).await;
        assert!(none.symbols().snapshot().is_none());

        let offline = runtime();
        connect(&offline, "Demo", false);
        let state = state_with(Some(offline));
        assert_eq!(
            refresh(&state).await.expect_err("offline"),
            "the terminal is not connected"
        );
        maintain(&state).await;
        assert!(
            state.symbols().snapshot().is_none(),
            "nothing pulled while offline"
        );
    }

    #[actix_web::test]
    async fn refresh_pulls_every_page_from_the_terminal_and_stores_it() {
        let broker = runtime();
        connect(&broker, "Demo-Server", true);
        let link = broker.ea_link().expect("ea link");
        let state = state_with(Some(broker));

        let stored = with_terminal(refresh(&state), serve_pages(link, 250, 2)).await;
        assert_eq!(stored.expect("refreshes"), 250);

        let held = state.symbols().snapshot().expect("stored");
        assert_eq!(held.server(), "Demo-Server");
        assert_eq!(held.entries().len(), 250);
        assert!(
            held.entries()
                .iter()
                .any(|entry| entry.name == "EURUSD" && entry.category == SymbolCategory::Forex)
        );
        assert!(state.symbols().begin().is_some(), "the guard was released");
    }

    #[actix_web::test]
    async fn refresh_rejects_an_empty_listing_and_a_concurrent_pull() {
        let broker = runtime();
        connect(&broker, "Demo", true);
        let link = broker.ea_link().expect("ea link");
        let state = state_with(Some(broker));

        let held = state.symbols().begin().expect("claim");
        assert_eq!(
            refresh(&state).await.expect_err("busy"),
            "a refresh is already running"
        );
        drop(held);

        let terminal = link.clone();
        let empty = async move {
            while !terminal.answer_next_symbol_page(|offset, _| {
                Ok(json!({ "total": 0, "offset": offset, "symbols": [] }))
            }) {
                actix_web::rt::time::sleep(Duration::from_millis(1)).await;
            }
        };
        let result = with_terminal(refresh(&state), empty).await;
        assert_eq!(
            result.expect_err("empty"),
            "the terminal listed no tradable instruments"
        );
        assert!(state.symbols().snapshot().is_none());
    }

    #[actix_web::test]
    async fn maintenance_pulls_once_then_stays_quiet_until_something_changes() {
        let broker = runtime();
        connect(&broker, "Demo", true);
        let link = broker.ea_link().expect("ea link");
        let state = state_with(Some(broker));

        with_terminal(maintain(&state), serve_pages(link.clone(), 40, 1)).await;
        assert_eq!(
            state.symbols().snapshot().expect("pulled").entries().len(),
            40
        );

        // Current catalogue, same server: no new command is queued.
        maintain(&state).await;
        assert!(
            !link.answer_next_symbol_page(|_, _| Err("unexpected".to_owned())),
            "an up-to-date catalogue is not pulled again"
        );

        // The account moved to another server: pulled again.
        reconnect(&link, "Other-Server");
        with_terminal(maintain(&state), serve_pages(link.clone(), 10, 1)).await;
        let held = state.symbols().snapshot().expect("re-pulled");
        assert_eq!((held.server(), held.entries().len()), ("Other-Server", 10));
    }

    /// Records a fresh snapshot on a link that is already shared.
    fn reconnect(link: &crate::broker::EaLink, server: &str) {
        link.record(AccountSnapshot::new(
            AccountLogin::parse(94168).expect("login"),
            ServerName::parse(server).expect("server"),
            Symbol::parse("EURUSD").expect("symbol"),
            true,
            true,
            0,
            0.0,
        ));
    }

    #[actix_web::test]
    async fn a_failed_pull_backs_off_then_retries() {
        let broker = runtime();
        connect(&broker, "Demo", true);
        let link = broker.ea_link().expect("ea link");
        let t0 = UNIX_EPOCH + Duration::from_secs(2_000_000);
        let state = state_with(Some(broker)).with_fixed_now(Some(t0));

        let terminal = link.clone();
        let failing = async move {
            while !terminal.answer_next_symbol_page(|_, _| Err("old EA build".to_owned())) {
                actix_web::rt::time::sleep(Duration::from_millis(1)).await;
            }
        };
        with_terminal(maintain(&state), failing).await;
        assert!(state.symbols().snapshot().is_none());
        assert_eq!(
            state.symbols().last_failure().as_deref(),
            Some("old EA build")
        );

        // Moments later: backing off, so nothing is queued.
        maintain(&state).await;
        assert!(!link.answer_next_symbol_page(|_, _| Err("unexpected".to_owned())));

        // After the back-off the pull is attempted again, and succeeds.
        let later = state.clone().with_fixed_now(Some(t0 + RETRY_BACKOFF));
        with_terminal(maintain(&later), serve_pages(link.clone(), 5, 1)).await;
        assert_eq!(
            state.symbols().snapshot().expect("pulled").entries().len(),
            5
        );
        assert!(state.symbols().last_failure().is_none());
    }

    #[actix_web::test]
    async fn the_route_reports_not_ready_then_the_grouped_catalogue() {
        let state = state_with(None);
        let app = actix_web::test::init_service(crate::app::create_app(state.clone())).await;

        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri("/symbols")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = actix_web::test::read_body_json(response).await;
        assert_eq!(body["ready"], false);
        assert_eq!(body["reason"], "waiting for the terminal");
        assert_eq!(body["symbols"], json!([]));

        state
            .symbols()
            .record_failure(UNIX_EPOCH, "terminal is silent");
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri("/symbols")
                .to_request(),
        )
        .await;
        let body: Value = actix_web::test::read_body_json(response).await;
        assert_eq!(body["reason"], "terminal is silent");

        state.symbols().store(snapshot(
            "Demo-Server",
            UNIX_EPOCH + Duration::from_secs(99),
        ));
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri("/symbols")
                .to_request(),
        )
        .await;
        let body: Value = actix_web::test::read_body_json(response).await;
        assert_eq!(body["ready"], true);
        assert_eq!(body["server"], "Demo-Server");
        assert_eq!(body["fetchedAt"], 99);
        assert_eq!(body["total"], 3);
        assert_eq!(
            body["categories"],
            json!([
                {"id": "forex", "label": "Forex", "count": 2},
                {"id": "indices", "label": "Indices", "count": 1}
            ])
        );
        let first = &body["symbols"][0];
        assert_eq!(first["name"], "EURUSD");
        assert_eq!(first["category"], "forex");
        assert_eq!(first["path"], "Forex\\EURUSD");
        // The default policy allows no instrument, and the route says so.
        assert_eq!(first["riskAllowed"], false);
    }

    #[actix_web::test]
    async fn the_refresh_route_reports_success_and_failure() {
        let without = actix_web::test::init_service(crate::app::create_app(state_with(None))).await;
        let response = actix_web::test::call_service(
            &without,
            actix_web::test::TestRequest::post()
                .uri("/symbols/refresh")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
        let body: Value = actix_web::test::read_body_json(response).await;
        assert_eq!(body["error"], "refresh_failed");
        assert_eq!(body["reason"], "no broker is configured");

        let broker = runtime();
        connect(&broker, "Demo", true);
        let link = broker.ea_link().expect("ea link");
        let state = state_with(Some(broker));
        let app = actix_web::test::init_service(crate::app::create_app(state)).await;
        let request = actix_web::test::TestRequest::post()
            .uri("/symbols/refresh")
            .to_request();
        let response = with_terminal(
            actix_web::test::call_service(&app, request),
            serve_pages(link, 7, 1),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = actix_web::test::read_body_json(response).await;
        assert_eq!(
            (body["status"].as_str(), body["count"].as_u64()),
            (Some("refreshed"), Some(7))
        );
    }

    #[test]
    fn repeated_failures_wait_longer_each_time_up_to_a_cap() {
        let catalog = SymbolCatalog::new();
        let t0 = UNIX_EPOCH + Duration::from_secs(9_000);
        // Waits after the 1st..6th failure in a row: 1, 2, 4, 8, then the 10 minute cap.
        for wait_secs in [60_u64, 120, 240, 480, 600, 600] {
            catalog.record_failure(t0, "unsupported command");
            assert!(
                catalog.in_backoff(t0 + Duration::from_secs(wait_secs - 1)),
                "still waiting just before {wait_secs}s"
            );
            assert!(
                !catalog.in_backoff(t0 + Duration::from_secs(wait_secs)),
                "free again at {wait_secs}s"
            );
        }
        // A success resets the streak, so the next failure starts at one minute again.
        catalog.store(snapshot("Demo", t0));
        catalog.record_failure(t0, "again");
        assert!(!catalog.in_backoff(t0 + RETRY_BACKOFF));
    }

    #[test]
    fn only_one_refresh_runs_at_a_time() {
        let catalog = SymbolCatalog::new();
        let first = catalog.begin().expect("first claim");
        assert!(catalog.begin().is_none(), "second claim while running");
        drop(first);
        assert!(catalog.begin().is_some(), "free again once finished");
    }
}
