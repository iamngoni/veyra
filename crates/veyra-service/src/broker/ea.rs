//! EA control channel — the first [`BrokerLink`](crate::broker::BrokerLink)
//! implementation.
//!
//! Veyra hosts a loopback-only HTTP endpoint; the MetaTrader 4 EA polls it,
//! presenting a shared token. This module records the venue state the EA
//! reports, answers the probe protocol (`ping` requests a `pong`), and carries
//! the idempotent command queue: commands are delivered on a poll, executed by
//! the EA, and acknowledged by stable id. No command places, modifies, or
//! cancels an order: `order_check` only asks the terminal to validate a
//! request, and mutating commands arrive later behind the risk gate with the
//! same id/ack discipline. MQL4 has no socket API, so HTTP through the
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
use crate::trading::intent::TradeIntent;

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

    /// Parses an externally supplied identifier (for example a URL path).
    pub fn parse(value: &str) -> Option<Self> {
        Uuid::parse_str(value).ok().map(Self)
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

/// Commands the EA can execute. None of them places, modifies, or cancels an
/// order; `order_check` only asks the terminal to validate a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    /// Return-path check with no payload.
    Ping,
    /// Report account state (balance, equity, free margin, order count).
    AccountSnapshot,
    /// Ask the terminal to validate an order request without sending it.
    OrderCheck,
}

impl CommandKind {
    /// Returns the stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::AccountSnapshot => "account_snapshot",
            Self::OrderCheck => "order_check",
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

/// Terminal verdict for an `order_check` command: the request passed, or the
/// classic MT4 trade code (131 volume, 134 money, 130 stops, 133 disabled)
/// that would reject it. No order exists in the venue; the terminal applies
/// its own market rules and margin engine.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OrderCheckPayload {
    /// Whether the terminal accepted the request in principle.
    pub passed: bool,
    /// Classic MT4 trade code; zero when the check passed.
    pub retcode: i64,
    /// Broker explanation, echoed for operators.
    pub comment: String,
    /// Margin the venue would require for the order, in account currency.
    pub margin: f64,
}

impl OrderCheckPayload {
    /// Rejects values an operator must never act on.
    fn validate(&self) -> Result<(), String> {
        if !self.margin.is_finite() || self.margin < 0.0 {
            return Err("margin must be a finite, non-negative number".to_owned());
        }
        if self.comment.len() > 256 || self.comment.chars().any(char::is_control) {
            return Err(
                "comment must be at most 256 characters without control characters".to_owned(),
            );
        }
        Ok(())
    }
}

/// Order request sent to the EA for validation, derived only from an approved
/// intent. Fields mirror the intent wire contract so the EA can read them
/// without a nested parser.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EaOrderRequest {
    symbol: String,
    side: &'static str,
    order_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    price: Option<f64>,
    volume: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_loss: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    take_profit: Option<f64>,
}

impl EaOrderRequest {
    /// Maps an approved intent to the EA wire request.
    ///
    /// The intent type can only be minted by the risk gate, so an order
    /// request cannot be built from a raw draft.
    pub fn from_intent(intent: &TradeIntent) -> Self {
        let draft = intent.draft();
        Self {
            symbol: draft.symbol().as_str().to_owned(),
            side: draft.side().as_str(),
            order_type: draft.order().as_str(),
            price: draft.order().price().map(|price| price.value()),
            volume: draft.volume().value(),
            stop_loss: draft.stop_loss().map(|price| price.value()),
            take_profit: draft.take_profit().map(|price| price.value()),
        }
    }
}

/// Typed result of a completed command.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandPayload {
    /// `ping` carries no payload.
    Ping,
    /// Result of `account_snapshot`.
    AccountSnapshot(AccountSnapshotPayload),
    /// Result of `order_check`; never an executed order.
    OrderCheck(OrderCheckPayload),
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
    #[serde(default)]
    orders: Option<u32>,
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
        let open_orders = self.orders.ok_or(BrokerError::InvalidPayload {
            field: "orders",
            reason: "missing",
        })?;
        Ok(AccountSnapshot::new(
            login,
            server,
            symbol,
            self.connected.unwrap_or(false),
            self.trade_allowed.unwrap_or(false),
            open_orders,
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
        /// Present only for commands that carry a request payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        order: Option<EaOrderRequest>,
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
    request: Option<EaOrderRequest>,
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
    last_account: Mutex<Option<AccountSnapshotPayload>>,
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
            last_account: Mutex::new(None),
        }
    }

    /// Queues a payload-free command. It is delivered on the EA's next poll
    /// and re-delivered until acknowledged (at-least-once delivery).
    pub fn enqueue(&self, kind: CommandKind) -> CommandId {
        self.enqueue_with(kind, None)
    }

    /// Queues a broker-side order validation.
    ///
    /// Takes an [`EaOrderRequest`], which can only be derived from an approved
    /// intent, so no raw draft can reach the terminal through this path.
    pub fn enqueue_order_check(&self, request: EaOrderRequest) -> CommandId {
        self.enqueue_with(CommandKind::OrderCheck, Some(request))
    }

    fn enqueue_with(&self, kind: CommandKind, request: Option<EaOrderRequest>) -> CommandId {
        let id = CommandId::new();
        self.with_commands(|queue| {
            queue.push_back(EaCommand {
                id,
                kind,
                state: CommandState::Pending,
                issued_at: Instant::now(),
                request,
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
    /// command for delivery, including its request payload when one exists.
    fn deliverable(&self) -> Option<(CommandId, CommandKind, Option<EaOrderRequest>)> {
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
                .map(|command| (command.id, command.kind, command.request.clone()))
        })
    }

    /// Applies an acknowledgement; unknown ids and duplicate acks are ignored,
    /// so repeated delivery can never double-apply a result. A validated
    /// account snapshot is retained outside the command queue for callers that
    /// need the latest venue state (risk facts, reconciliation).
    fn apply_ack(&self, ack: &EaAck) {
        let retained = self.with_commands(|queue| {
            let command = queue.iter_mut().find(|command| command.id == ack.id)?;
            if !matches!(command.state, CommandState::Pending) {
                return None;
            }
            if !ack.ok {
                command.state = CommandState::Failed {
                    reason: ack
                        .error
                        .clone()
                        .unwrap_or_else(|| "acknowledged failure".to_owned()),
                };
                return None;
            }
            match payload_for(command.kind, ack.data.clone()) {
                Ok(payload) => {
                    let retained = match &payload {
                        CommandPayload::AccountSnapshot(snapshot) => Some(snapshot.clone()),
                        CommandPayload::Ping | CommandPayload::OrderCheck(_) => None,
                    };
                    command.state = CommandState::Completed { payload };
                    retained
                }
                Err(reason) => {
                    command.state = CommandState::Failed { reason };
                    None
                }
            }
        });
        if let Some(snapshot) = retained {
            self.with_last_account(|slot| *slot = Some(snapshot));
        }
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

    /// Latest validated `account_snapshot` acknowledgement, if any.
    ///
    /// `None` until the first snapshot command completes. Read-only callers
    /// (risk facts, reconciliation) use it instead of querying the terminal.
    pub fn last_account(&self) -> Option<AccountSnapshotPayload> {
        self.with_last_account(|slot| slot.clone())
    }

    fn with_last_account<T>(
        &self,
        apply: impl FnOnce(&mut Option<AccountSnapshotPayload>) -> T,
    ) -> T {
        // Same poisoning stance as the other guards: the slot holds one whole
        // value, so recovering the guard cannot observe a partial write.
        let mut guard = match self.last_account.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply(&mut guard)
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
                    Some((id, kind, order)) => EaReply::Command { id, kind, order },
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
        CommandKind::OrderCheck => {
            let value = data.ok_or_else(|| "order_check ack is missing data".to_owned())?;
            let payload: OrderCheckPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid order_check payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::OrderCheck(payload))
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
    use std::time::Duration;

    use super::{
        EaAck, EaLink, EaOrderRequest, EaToken, OrderCheckPayload, hex_preview, payload_for,
    };
    use crate::broker::ea::{
        AccountSnapshotPayload, CommandId, CommandKind, CommandPayload, CommandState,
    };
    use crate::trading::intent::{
        OrderKind, Price, Side, TradeIntent, TradeIntentDraft, Volume, parse_instrument,
    };

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
        assert_eq!(CommandKind::OrderCheck.as_str(), "order_check");
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

        let check = payload_for(
            CommandKind::OrderCheck,
            Some(serde_json::json!({
                "passed": true,
                "retcode": 0,
                "comment": "Done",
                "margin": 2.19
            })),
        )
        .expect("valid order check payload");
        assert_eq!(
            check,
            CommandPayload::OrderCheck(OrderCheckPayload {
                passed: true,
                retcode: 0,
                comment: "Done".to_owned(),
                margin: 2.19,
            })
        );

        let missing = payload_for(CommandKind::OrderCheck, None).expect_err("missing data");
        assert!(
            missing.contains("missing data"),
            "unexpected error: {missing}"
        );

        let negative = payload_for(
            CommandKind::OrderCheck,
            Some(serde_json::json!({
                "passed": false,
                "retcode": 10019,
                "comment": "no money",
                "margin": -1.0
            })),
        )
        .expect_err("negative margin must be rejected");
        assert!(negative.contains("margin"), "unexpected error: {negative}");
    }

    #[test]
    fn order_requests_map_only_from_approved_intents() {
        let draft = TradeIntentDraft::new(
            parse_instrument("eurusd").expect("symbol"),
            Side::Buy,
            OrderKind::Market,
            Volume::parse(0.01).expect("volume"),
            None,
            None,
            None,
        );
        let intent = TradeIntent::approve(draft);
        let request = EaOrderRequest::from_intent(&intent);
        let wire = serde_json::to_value(&request).expect("serializable");
        assert_eq!(wire["symbol"], "EURUSD");
        assert_eq!(wire["side"], "buy");
        assert_eq!(wire["order_type"], "market");
        assert_eq!(wire["volume"], 0.01);
        assert!(wire.get("price").is_none(), "market orders carry no price");
        assert!(wire.get("stop_loss").is_none());

        let limit = TradeIntentDraft::new(
            parse_instrument("EURUSD").expect("symbol"),
            Side::Sell,
            OrderKind::Limit(Price::parse(1.2).expect("price")),
            Volume::parse(0.02).expect("volume"),
            Some(Price::parse(1.25).expect("price")),
            None,
            None,
        );
        let wire = serde_json::to_value(EaOrderRequest::from_intent(&TradeIntent::approve(limit)))
            .expect("serializable");
        assert_eq!(wire["order_type"], "limit");
        assert_eq!(wire["price"], 1.2);
        assert_eq!(wire["stop_loss"], 1.25);
    }

    #[test]
    fn order_checks_are_delivered_with_their_request() {
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        let intent = TradeIntent::approve(TradeIntentDraft::new(
            parse_instrument("EURUSD").expect("symbol"),
            Side::Buy,
            OrderKind::Market,
            Volume::parse(0.01).expect("volume"),
            None,
            None,
            None,
        ));
        let id = link.enqueue_order_check(EaOrderRequest::from_intent(&intent));
        let (delivered_id, kind, order) = link.deliverable().expect("pending command");
        assert_eq!(delivered_id, id);
        assert_eq!(kind, CommandKind::OrderCheck);
        assert_eq!(order, Some(EaOrderRequest::from_intent(&intent)));

        let record = link.command(id).expect("record");
        assert_eq!(record.state, CommandState::Pending);
    }

    #[test]
    fn command_ids_parse_from_strings() {
        let id = CommandId::new();
        assert_eq!(CommandId::parse(&id.to_string()), Some(id));
        assert_eq!(CommandId::parse("not-a-uuid"), None);
    }

    #[test]
    fn validated_snapshot_acks_are_retained_for_risk_facts() {
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        assert!(link.last_account().is_none(), "nothing retained yet");

        let id = link.enqueue(CommandKind::AccountSnapshot);
        link.apply_ack(&EaAck {
            id,
            ok: true,
            data: Some(serde_json::json!({
                "balance": 20.57,
                "equity": 20.57,
                "freeMargin": 20.57,
                "orders": 3,
                "serverTime": 1_758_000_000
            })),
            error: None,
        });

        let retained = link.last_account().expect("snapshot retained");
        assert_eq!(retained.orders, 3);
        assert_eq!(
            link.command(id).expect("recorded").state,
            CommandState::Completed {
                payload: CommandPayload::AccountSnapshot(retained)
            }
        );

        let failed = link.enqueue(CommandKind::AccountSnapshot);
        link.apply_ack(&EaAck {
            id: failed,
            ok: false,
            data: None,
            error: Some("nope".to_owned()),
        });
        assert_eq!(link.last_account().expect("previous retained").orders, 3);
    }
}
