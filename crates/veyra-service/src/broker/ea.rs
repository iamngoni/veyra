//! EA control channel — the first [`BrokerLink`](crate::broker::BrokerLink)
//! implementation.
//!
//! Veyra hosts a loopback-only HTTP endpoint; the MetaTrader 4 EA polls it,
//! presenting a shared token. This module records the venue state the EA
//! reports and answers the probe protocol (`ping` requests a `pong`). Order
//! commands are not implemented yet; when they are, they travel as an
//! idempotent command queue over this same channel. MQL4 has no socket API,
//! so HTTP through the terminal's `WebRequest` client is the transport.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use actix_web::body::BoxBody;
use actix_web::dev::{ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::{App, Error, HttpResponse, post, web};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::broker::settings::EaToken;
use crate::broker::{
    AccountLogin, AccountSnapshot, BrokerError, BrokerLink, BrokerProvider, LinkReport, ServerName,
    Symbol,
};

/// Message kinds accepted from the EA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EaKind {
    /// First message after connect.
    Hello,
    /// Periodic heartbeat.
    Hb,
    /// Acknowledgement of a ping.
    Pong,
}

/// One poll from the EA.
///
/// Unknown extra fields are ignored so a newer EA can add fields without
/// breaking an older service.
#[derive(Debug, Deserialize)]
pub struct EaPoll {
    #[serde(rename = "t")]
    kind: EaKind,
    token: String,
    #[serde(default)]
    acct: Option<i64>,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    connected: Option<bool>,
    #[serde(rename = "tradeAllowed", default)]
    trade_allowed: Option<bool>,
}

impl EaPoll {
    fn token(&self) -> &str {
        &self.token
    }

    fn kind(&self) -> EaKind {
        self.kind
    }

    /// Validates the reported fields into a domain snapshot.
    fn snapshot(&self) -> Result<AccountSnapshot, BrokerError> {
        let login = AccountLogin::parse(self.acct.ok_or(BrokerError::InvalidPayload {
            field: "acct",
            reason: "missing",
        })?)?;
        let server =
            ServerName::parse(self.server.as_deref().ok_or(BrokerError::InvalidPayload {
                field: "server",
                reason: "missing",
            })?)?;
        let symbol = Symbol::parse(self.symbol.as_deref().ok_or(BrokerError::InvalidPayload {
            field: "symbol",
            reason: "missing",
        })?)?;
        Ok(AccountSnapshot::new(
            login,
            server,
            symbol,
            self.connected.unwrap_or(false),
            self.trade_allowed.unwrap_or(false),
        ))
    }
}

/// Reply sent to the EA: `ping` requests a `pong`, `none` means nothing to do.
#[derive(Debug, Serialize)]
pub struct EaReply {
    t: &'static str,
}

impl EaReply {
    fn ping() -> Self {
        Self { t: "ping" }
    }

    fn none() -> Self {
        Self { t: "none" }
    }
}

/// Stable machine-readable error body.
#[derive(Debug, Serialize)]
pub struct EaErrorBody {
    t: &'static str,
    code: &'static str,
}

impl EaErrorBody {
    fn new(code: &'static str) -> Self {
        Self { t: "error", code }
    }
}

#[derive(Debug)]
struct EaState {
    snapshot: AccountSnapshot,
    last_seen: SystemTime,
}

/// Shared state of the EA control channel.
#[derive(Debug)]
pub struct EaLink {
    token: EaToken,
    stale_after: Duration,
    state: Mutex<Option<EaState>>,
    pongs: AtomicU64,
}

impl EaLink {
    /// Builds a link accepting `token`, reporting state older than
    /// `stale_after` as stale.
    pub fn new(token: EaToken, stale_after: Duration) -> Self {
        Self {
            token,
            stale_after,
            state: Mutex::new(None),
            pongs: AtomicU64::new(0),
        }
    }

    /// Constant-time comparison of a presented token with the configured one.
    pub fn token_matches(&self, presented: &str) -> bool {
        bool::from(self.token.expose().as_bytes().ct_eq(presented.as_bytes()))
    }

    /// Records a validated snapshot as the latest venue state.
    pub fn record(&self, snapshot: AccountSnapshot) {
        self.with_state(|slot| {
            *slot = Some(EaState {
                snapshot,
                last_seen: SystemTime::now(),
            });
        });
    }

    /// Counts a pong acknowledgement.
    pub fn record_pong(&self) {
        self.pongs.fetch_add(1, Ordering::Relaxed);
    }

    /// Pong acknowledgements received since start; a live-channel probe metric.
    pub fn pongs_received(&self) -> u64 {
        self.pongs.load(Ordering::Relaxed)
    }

    fn with_state<T>(&self, apply: impl FnOnce(&mut Option<EaState>) -> T) -> T {
        // Poisoning cannot make this state unsound to reuse: every writer
        // replaces the whole value under the lock and readers only clone it.
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply(&mut guard)
    }
}

#[async_trait]
impl BrokerLink for EaLink {
    fn provider(&self) -> BrokerProvider {
        BrokerProvider::Ea
    }

    async fn report(&self) -> LinkReport {
        let entry = self.with_state(|slot| {
            slot.as_ref()
                .map(|state| (state.snapshot.clone(), state.last_seen))
        });
        match entry {
            None => LinkReport {
                snapshot: None,
                fresh: false,
            },
            Some((snapshot, last_seen)) => {
                let fresh = match SystemTime::now().duration_since(last_seen) {
                    Ok(age) => age <= self.stale_after,
                    Err(_) => false,
                };
                LinkReport {
                    snapshot: Some(snapshot),
                    fresh,
                }
            }
        }
    }
}

#[post("/ea/poll")]
/// Accepts one EA poll after authenticating the shared token.
///
/// The body is read as raw bytes and parsed here because the MQL4 WebRequest
/// client cannot set a JSON content type; requiring one would reject every
/// real EA poll.
pub async fn poll(payload: web::Bytes, link: web::Data<EaLink>) -> HttpResponse {
    let poll: EaPoll = match serde_json::from_slice(&payload) {
        Ok(poll) => poll,
        Err(error) => {
            tracing::warn!(%error, "rejected EA poll: malformed JSON");
            return HttpResponse::BadRequest().json(EaErrorBody::new("malformed_json"));
        }
    };

    if !link.token_matches(poll.token()) {
        return HttpResponse::Unauthorized().json(EaErrorBody::new("unauthorized"));
    }

    match poll.kind() {
        EaKind::Pong => {
            link.record_pong();
            HttpResponse::Ok().json(EaReply::none())
        }
        EaKind::Hello | EaKind::Hb => match poll.snapshot() {
            Ok(snapshot) => {
                link.record(snapshot);
                let reply = if matches!(poll.kind(), EaKind::Hello) {
                    EaReply::ping()
                } else {
                    EaReply::none()
                };
                HttpResponse::Ok().json(reply)
            }
            Err(error) => {
                tracing::warn!(%error, "rejected EA poll with invalid payload");
                HttpResponse::BadRequest().json(EaErrorBody::new("invalid_payload"))
            }
        },
    }
}

/// Builds the EA-facing Actix application.
pub fn create_ea_app(
    link: Arc<EaLink>,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<BoxBody>,
        Error = Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(web::Data::from(link))
        .wrap(actix_web::middleware::DefaultHeaders::new().add(("Cache-Control", "no-store")))
        .service(poll)
}

/// Builds the loopback server that serves the EA channel.
///
/// # Errors
/// Returns IO errors from binding `address`.
pub fn build_server(
    link: Arc<EaLink>,
    address: SocketAddr,
) -> std::io::Result<actix_web::dev::Server> {
    let server = actix_web::HttpServer::new(move || create_ea_app(link.clone()))
        .workers(1)
        .shutdown_timeout(5)
        .bind(address)?
        .run();
    tracing::info!(%address, "EA control channel listening (loopback only)");
    Ok(server)
}
