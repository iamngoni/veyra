//! EA control channel — the first [`BrokerLink`](crate::broker::BrokerLink)
//! implementation.
//!
//! Veyra hosts a loopback-only HTTP endpoint; the MetaTrader 4 EA polls it,
//! presenting a shared token. This module records the venue state the EA
//! reports, answers the probe protocol (`ping` requests a `pong`), and carries
//! the idempotent command queue: commands are delivered on a poll, executed by
//! the EA, and acknowledged by stable id. Only read-only commands exist today
//! (`ping`, `account_snapshot`); mutating commands arrive later and must keep
//! the same id/ack discipline. MQL4 has no socket API, so HTTP through the
//! terminal's `WebRequest` client is the transport.

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use actix_web::body::BoxBody;
use actix_web::dev::{ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::{App, Error, HttpResponse, post, web};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use uuid::Uuid;

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
    /// Acknowledgement of a command.
    Ack,
}

/// How many finished commands stay queryable.
const COMMAND_HISTORY: usize = 64;

/// Stable identifier for one command; identical across delivery and ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(Uuid);

impl CommandId {
    /// Generates a fresh random identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Commands the EA can execute. Only read-only commands exist today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    /// Return-path check with no payload.
    Ping,
    /// Report account state (balance, equity, free margin, order count).
    AccountSnapshot,
}

impl CommandKind {
    /// Returns the stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::AccountSnapshot => "account_snapshot",
        }
    }
}

/// Account state reported by an `account_snapshot` command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountSnapshotPayload {
    /// Account balance.
    pub balance: f64,
    /// Account equity.
    pub equity: f64,
    /// Free margin.
    #[serde(rename = "freeMargin")]
    pub free_margin: f64,
    /// Open orders; MT4 counts positions and pending orders here.
    pub orders: u32,
    /// Terminal server time.
    #[serde(rename = "serverTime")]
    pub server_time: i64,
}

impl AccountSnapshotPayload {
    /// Rejects non-finite money values before they reach callers.
    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("balance", self.balance),
            ("equity", self.equity),
            ("freeMargin", self.free_margin),
        ] {
            if !value.is_finite() {
                return Err(format!("{name} must be a finite number"));
            }
        }
        Ok(())
    }
}

/// Typed result of a completed command.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandPayload {
    /// `ping` carries no payload.
    Ping,
    /// Result of `account_snapshot`.
    AccountSnapshot(AccountSnapshotPayload),
}

/// Lifecycle state of one command.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandState {
    /// Delivered (or awaiting delivery) and not yet acknowledged.
    Pending,
    /// Acknowledged successfully with a validated payload.
    Completed {
        /// Validated result payload.
        payload: CommandPayload,
    },
    /// Timed out or acknowledged as failed.
    Failed {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// One command as seen by callers and tests.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandRecord {
    /// Identifier.
    pub id: CommandId,
    /// Requested kind.
    pub kind: CommandKind,
    /// Current state.
    pub state: CommandState,
}

/// Acknowledgement sent by the EA.
#[derive(Debug, Deserialize)]
struct EaAck {
    id: CommandId,
    ok: bool,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    error: Option<String>,
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
    #[serde(rename = "id", default)]
    command_id: Option<CommandId>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

impl EaPoll {
    fn token(&self) -> &str {
        &self.token
    }

    fn kind(&self) -> EaKind {
        self.kind
    }

    /// Builds an acknowledgement view when the message carries the flat ack
    /// fields (`id`, `ok`, optional `data`/`error`) the EA sends.
    fn ack(&self) -> Option<EaAck> {
        match (self.command_id, self.ok) {
            (Some(id), Some(ok)) => Some(EaAck {
                id,
                ok,
                data: self.data.clone(),
                error: self.error.clone(),
            }),
            _ => None,
        }
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

/// Reply sent to the EA: `ping` requests a `pong`, `cmd` hands over a command,
/// `none` means nothing to do.
#[derive(Debug, Serialize)]
#[serde(tag = "t")]
pub enum EaReply {
    /// Nothing to do.
    #[serde(rename = "none")]
    None,
    /// Return-path check.
    #[serde(rename = "ping")]
    Ping,
    /// A command for the EA to execute.
    #[serde(rename = "cmd")]
    Command {
        /// Stable command id echoed back in the ack.
        id: CommandId,
        /// Command to execute.
        kind: CommandKind,
    },
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

#[derive(Debug)]
struct EaCommand {
    id: CommandId,
    kind: CommandKind,
    state: CommandState,
    issued_at: Instant,
}

/// Shared state of the EA control channel.
#[derive(Debug)]
pub struct EaLink {
    token: EaToken,
    stale_after: Duration,
    command_timeout: Duration,
    state: Mutex<Option<EaState>>,
    commands: Mutex<VecDeque<EaCommand>>,
    pongs: AtomicU64,
}

impl EaLink {
    /// Builds a link accepting `token`, reporting state older than
    /// `stale_after` as stale, and failing commands that stay unacknowledged
    /// for longer than `command_timeout`.
    pub fn new(token: EaToken, stale_after: Duration, command_timeout: Duration) -> Self {
        Self {
            token,
            stale_after,
            command_timeout,
            state: Mutex::new(None),
            commands: Mutex::new(VecDeque::new()),
            pongs: AtomicU64::new(0),
        }
    }

    /// Queues a command. It is delivered on the EA's next poll and re-delivered
    /// until acknowledged (at-least-once delivery); read-only commands make
    /// that safe, and mutating commands must stay idempotent per id.
    pub fn enqueue(&self, kind: CommandKind) -> CommandId {
        let id = CommandId::new();
        self.with_commands(|queue| {
            queue.push_back(EaCommand {
                id,
                kind,
                state: CommandState::Pending,
                issued_at: Instant::now(),
            });
            while queue.len() > COMMAND_HISTORY {
                queue.pop_front();
            }
        });
        id
    }

    /// Returns the current record for a command still inside the bounded
    /// history.
    pub fn command(&self, id: CommandId) -> Option<CommandRecord> {
        self.with_commands(|queue| {
            queue
                .iter()
                .find(|command| command.id == id)
                .map(|command| CommandRecord {
                    id: command.id,
                    kind: command.kind,
                    state: command.state.clone(),
                })
        })
    }

    /// Marks timed-out commands as failed and returns the oldest pending
    /// command for delivery.
    fn deliverable(&self) -> Option<(CommandId, CommandKind)> {
        let timeout = self.command_timeout;
        self.with_commands(|queue| {
            let now = Instant::now();
            for command in queue.iter_mut() {
                if matches!(command.state, CommandState::Pending)
                    && now.duration_since(command.issued_at) > timeout
                {
                    command.state = CommandState::Failed {
                        reason: "timeout".to_owned(),
                    };
                }
            }
            queue
                .iter()
                .find(|command| matches!(command.state, CommandState::Pending))
                .map(|command| (command.id, command.kind))
        })
    }

    /// Applies an acknowledgement; unknown ids and duplicate acks are ignored,
    /// so repeated delivery can never double-apply a result.
    fn apply_ack(&self, ack: &EaAck) {
        self.with_commands(|queue| {
            let Some(command) = queue.iter_mut().find(|command| command.id == ack.id) else {
                return;
            };
            if !matches!(command.state, CommandState::Pending) {
                return;
            }
            command.state = if ack.ok {
                match payload_for(command.kind, ack.data.clone()) {
                    Ok(payload) => CommandState::Completed { payload },
                    Err(reason) => CommandState::Failed { reason },
                }
            } else {
                CommandState::Failed {
                    reason: ack
                        .error
                        .clone()
                        .unwrap_or_else(|| "acknowledged failure".to_owned()),
                }
            };
        });
    }

    fn with_commands<T>(&self, apply: impl FnOnce(&mut VecDeque<EaCommand>) -> T) -> T {
        // Same poisoning stance as `with_state`: writers replace whole values,
        // readers clone, so recovering the guard is safe.
        let mut guard = match self.commands.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply(&mut guard)
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
    // Some WebRequest clients append terminating NUL bytes; strip them so a
    // well-formed body is never rejected for padding alone.
    let mut bytes: &[u8] = &payload;
    while bytes.last() == Some(&0) {
        bytes = &bytes[..bytes.len() - 1];
    }

    let poll: EaPoll = match serde_json::from_slice(bytes) {
        Ok(poll) => poll,
        Err(error) => {
            // Build the preview eagerly: tracing evaluates fields lazily, and
            // this diagnostic must exist even when no subscriber is installed.
            let head = hex_preview(&payload[..payload.len().min(24)]);
            let tail = hex_preview(&payload[payload.len().saturating_sub(24)..]);
            tracing::warn!(
                %error,
                len = payload.len(),
                %head,
                %tail,
                "rejected EA poll: malformed JSON"
            );
            return HttpResponse::BadRequest().json(EaErrorBody::new("malformed_json"));
        }
    };

    if !link.token_matches(poll.token()) {
        return HttpResponse::Unauthorized().json(EaErrorBody::new("unauthorized"));
    }

    // Acks may ride along with any message kind.
    if let Some(ack) = poll.ack() {
        link.apply_ack(&ack);
    }

    match poll.kind() {
        EaKind::Pong => {
            link.record_pong();
            HttpResponse::Ok().json(EaReply::None)
        }
        EaKind::Ack => HttpResponse::Ok().json(EaReply::None),
        EaKind::Hello | EaKind::Hb => match poll.snapshot() {
            Ok(snapshot) => {
                link.record(snapshot);
                let reply = match link.deliverable() {
                    Some((id, kind)) => EaReply::Command { id, kind },
                    // Ask for a pong on hello and until one has been seen for
                    // this process lifetime: it proves the return path before
                    // the channel is trusted with anything else.
                    None if matches!(poll.kind(), EaKind::Hello) || link.pongs_received() == 0 => {
                        EaReply::Ping
                    }
                    None => EaReply::None,
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

/// Validates an acknowledgement payload against its command kind.
fn payload_for(kind: CommandKind, data: Option<Value>) -> Result<CommandPayload, String> {
    match kind {
        CommandKind::Ping => Ok(CommandPayload::Ping),
        CommandKind::AccountSnapshot => {
            let value = data.ok_or_else(|| "account_snapshot ack is missing data".to_owned())?;
            let payload: AccountSnapshotPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid snapshot payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::AccountSnapshot(payload))
        }
    }
}

/// Hex preview used in malformed-payload diagnostics; never includes secrets by
/// itself, but payloads are operator-supplied control messages.
fn hex_preview(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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

#[cfg(test)]
mod tests {
    use super::{hex_preview, payload_for};
    use crate::broker::ea::{AccountSnapshotPayload, CommandId, CommandKind, CommandPayload};

    #[test]
    fn hex_preview_formats_bytes() {
        assert_eq!(hex_preview(&[0x7b, 0x22, 0x00]), "7b2200");
        assert_eq!(hex_preview(&[]), "");
    }

    #[test]
    fn command_ids_are_unique_and_defaultable() {
        let defaulted = CommandId::default();
        let generated = CommandId::new();
        assert_ne!(defaulted, generated);
        assert_eq!(defaulted.to_string().len(), 36);
    }

    #[test]
    fn command_names_are_stable() {
        assert_eq!(CommandKind::Ping.as_str(), "ping");
        assert_eq!(CommandKind::AccountSnapshot.as_str(), "account_snapshot");
    }

    #[test]
    fn non_finite_money_is_rejected() {
        let payload = AccountSnapshotPayload {
            balance: f64::NAN,
            equity: 0.0,
            free_margin: 0.0,
            orders: 0,
            server_time: 0,
        };
        let error = payload.validate().expect_err("NaN must be rejected");
        assert!(error.contains("balance"), "unexpected error: {error}");
    }

    #[test]
    fn payloads_are_validated_per_command_kind() {
        let error = payload_for(CommandKind::AccountSnapshot, None).expect_err("missing data");
        assert!(error.contains("missing data"), "unexpected error: {error}");
        assert_eq!(
            payload_for(CommandKind::Ping, None).expect("ping payload"),
            CommandPayload::Ping
        );
    }
}
