//! EA control channel — the first [`BrokerLink`](crate::broker::BrokerLink)
//! implementation.
//!
//! Veyra hosts a loopback HTTP endpoint by default; an explicit opt-in permits
//! binding it to an isolated container network. The MetaTrader 4 EA polls it,
//! presenting a shared token. This module records the venue state the EA
//! reports, answers the probe protocol (`ping` requests a `pong`), and carries
//! the idempotent command queue: commands are delivered on a poll, executed by
//! the EA, and acknowledged by stable id. No command places, modifies, or
//! cancels an order: `order_check` only asks the terminal to validate a
//! request, and mutating commands arrive later behind the risk gate with the
//! same id/ack discipline. MQL4 has no socket API, so HTTP through the
//! terminal's `WebRequest` client is the transport.

use std::collections::VecDeque;
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

use crate::audit::{AuditEvent, AuditKind, AuditRuntime};
use crate::broker::command::{
    AccountSnapshotPayload, CloseOrderRequest, CommandId, CommandKind, CommandPayload,
    CommandRecord, CommandState, ListedCommand, ModifyOrderRequest, OrderCheckPayload,
    OrderExecutionPayload, OrderHistoryPayload, OrderHistoryRequest, OrderRequest, PositionPayload,
    RatesPayload, RatesRequest, SymbolSpecPayload, SymbolSpecRequest,
};
use crate::broker::settings::EaToken;
use crate::broker::{
    AccountLogin, AccountSnapshot, BrokerError, BrokerLink, BrokerProvider, LinkReport,
    ORDER_MAGIC, ServerName, Symbol,
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
    #[serde(rename = "liveOrders", default)]
    live_orders: Option<bool>,
    #[serde(default)]
    orders: Option<u32>,
    #[serde(default)]
    lots: Option<f64>,
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
        let open_lots = match self.lots {
            Some(value) if value.is_finite() && value >= 0.0 => value,
            _ => {
                return Err(BrokerError::InvalidPayload {
                    field: "lots",
                    reason: "must be a finite, non-negative number",
                });
            }
        };
        Ok(AccountSnapshot::new(
            login,
            server,
            symbol,
            self.connected.unwrap_or(false),
            self.trade_allowed.unwrap_or(false),
            open_orders,
            open_lots,
        )
        .with_live_orders(self.live_orders.unwrap_or(false)))
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
        /// Present for order validation and execution commands.
        #[serde(skip_serializing_if = "Option::is_none")]
        order: Option<Box<OrderRequest>>,
        /// Present for close commands.
        #[serde(skip_serializing_if = "Option::is_none")]
        close: Option<Box<CloseOrderRequest>>,
        /// Present for stop-change commands.
        #[serde(skip_serializing_if = "Option::is_none")]
        modify: Option<Box<ModifyOrderRequest>>,
        /// Present for market-rates commands.
        #[serde(skip_serializing_if = "Option::is_none")]
        rates: Option<Box<RatesRequest>>,
        /// Present for instrument-contract commands.
        #[serde(skip_serializing_if = "Option::is_none")]
        spec: Option<Box<SymbolSpecRequest>>,
        /// Present for account-history commands.
        #[serde(skip_serializing_if = "Option::is_none")]
        history: Option<Box<OrderHistoryRequest>>,
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
    request: Option<CommandRequest>,
}

/// Payload a queued command carries, when it needs one.
#[derive(Debug, Clone, PartialEq)]
enum CommandRequest {
    /// Validation or execution request for a new order.
    Order(OrderRequest),
    /// Close request for a validated Veyra position.
    Close(CloseOrderRequest),
    /// Stop change for a validated Veyra position.
    Modify(ModifyOrderRequest),
    /// Market-rates request for one symbol and timeframe.
    Rates(RatesRequest),
    /// Instrument-contract request for one symbol.
    SymbolSpec(SymbolSpecRequest),
    /// Account-history request for one magic number and window.
    OrderHistory(OrderHistoryRequest),
}

/// Retained account snapshot plus the instant it was validated.
#[derive(Debug)]
struct StoredAccount {
    payload: AccountSnapshotPayload,
    at: SystemTime,
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
    last_account: Mutex<Option<StoredAccount>>,
    previous_account: Mutex<Option<StoredAccount>>,
    audit: Mutex<Option<Arc<AuditRuntime>>>,
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
            previous_account: Mutex::new(None),
            audit: Mutex::new(None),
        }
    }

    /// Queues a payload-free command. It is delivered on the EA's next poll
    /// and re-delivered until acknowledged (at-least-once delivery).
    pub fn enqueue(&self, kind: CommandKind) -> CommandId {
        self.enqueue_with(kind, None)
    }

    /// Queues a broker-side order validation.
    ///
    /// Takes an [`OrderRequest`], which can only be derived from an approved
    /// intent, so no raw draft can reach the terminal through this path.
    pub fn enqueue_order_check(&self, request: OrderRequest) -> CommandId {
        self.enqueue_with(
            CommandKind::OrderCheck,
            Some(CommandRequest::Order(request)),
        )
    }

    /// Queues a live order execution.
    ///
    /// Like [`Self::enqueue_order_check`], the request can only be derived from
    /// an approved intent. The terminal still refuses to trade until its own
    /// live-orders input is enabled, so real money needs two independent
    /// controls plus a gate approval.
    pub fn enqueue_order(&self, request: OrderRequest) -> CommandId {
        self.enqueue_with(CommandKind::OpenOrder, Some(CommandRequest::Order(request)))
    }

    /// Queues a close for one validated Veyra-owned ticket.
    pub fn enqueue_close(&self, request: CloseOrderRequest) -> CommandId {
        self.enqueue_with(
            CommandKind::CloseOrder,
            Some(CommandRequest::Close(request)),
        )
    }

    /// Queues a stop change for one validated Veyra-owned ticket.
    pub fn enqueue_modify(&self, request: ModifyOrderRequest) -> CommandId {
        self.enqueue_with(
            CommandKind::ModifyOrder,
            Some(CommandRequest::Modify(request)),
        )
    }

    /// Queues a read-only market-rates request.
    pub fn enqueue_rates(&self, request: RatesRequest) -> CommandId {
        self.enqueue_with(CommandKind::Rates, Some(CommandRequest::Rates(request)))
    }

    /// Queues a read-only instrument-contract request.
    pub fn enqueue_symbol_spec(&self, request: SymbolSpecRequest) -> CommandId {
        self.enqueue_with(
            CommandKind::SymbolSpec,
            Some(CommandRequest::SymbolSpec(request)),
        )
    }

    /// Queues a read-only account-history request.
    pub fn enqueue_order_history(&self, request: OrderHistoryRequest) -> CommandId {
        self.enqueue_with(
            CommandKind::OrderHistory,
            Some(CommandRequest::OrderHistory(request)),
        )
    }

    fn enqueue_with(&self, kind: CommandKind, request: Option<CommandRequest>) -> CommandId {
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

    /// Newest-first commands for the control surface, capped at `limit`.
    pub fn recent_commands(&self, limit: usize) -> Vec<ListedCommand> {
        self.with_commands(|queue| {
            queue
                .iter()
                .rev()
                .take(limit)
                .map(|command| match &command.state {
                    CommandState::Pending => ListedCommand {
                        id: command.id,
                        kind: command.kind,
                        status: "pending",
                        summary: None,
                        reason: None,
                    },
                    CommandState::Completed { payload } => ListedCommand {
                        id: command.id,
                        kind: command.kind,
                        status: "completed",
                        summary: Some(completed_summary(payload)),
                        reason: None,
                    },
                    CommandState::Failed { reason } => ListedCommand {
                        id: command.id,
                        kind: command.kind,
                        status: "failed",
                        summary: None,
                        reason: Some(reason.clone()),
                    },
                })
                .collect()
        })
    }

    /// Waits until a command reaches a terminal state, polling the retained
    /// queue. Delivery, acknowledgement, and timeout classification stay with
    /// the queue; this only observes. Commands that leave the bounded history
    /// or outlive `timeout` report a failure.
    pub async fn await_command(&self, id: CommandId, timeout: Duration) -> CommandState {
        let deadline = Instant::now() + timeout;
        loop {
            match self.command(id) {
                Some(record) => {
                    if !matches!(record.state, CommandState::Pending) {
                        return record.state;
                    }
                }
                None => {
                    return CommandState::Failed {
                        reason: "command left the retained history".to_owned(),
                    };
                }
            }
            if Instant::now() >= deadline {
                return CommandState::Failed {
                    reason: "await timeout".to_owned(),
                };
            }
            actix_web::rt::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Marks timed-out commands as failed and returns the oldest pending
    /// command for delivery, including its request payload when one exists.
    fn deliverable(&self) -> Option<(CommandId, CommandKind, Option<CommandRequest>)> {
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
                        CommandPayload::Ping
                        | CommandPayload::OrderCheck(_)
                        | CommandPayload::OpenOrder(_)
                        | CommandPayload::CloseOrder(_)
                        | CommandPayload::ModifyOrder(_)
                        | CommandPayload::Rates(_)
                        | CommandPayload::SymbolSpec(_)
                        | CommandPayload::OrderHistory(_) => None,
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
            let replaced = self.with_last_account(|slot| {
                let previous = slot.take();
                *slot = Some(StoredAccount {
                    payload: snapshot,
                    at: SystemTime::now(),
                });
                previous
            });
            if let Some(previous) = replaced {
                self.with_previous_account(|slot| *slot = Some(previous));
            }
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

    /// Attaches the audit trail; command acknowledgements are recorded
    /// best-effort from then on.
    pub fn set_audit(&self, audit: Arc<AuditRuntime>) {
        let mut guard = match self.audit.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(audit);
    }

    fn attached_audit(&self) -> Option<Arc<AuditRuntime>> {
        let guard = match self.audit.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    }

    /// Records the terminal state of an acknowledged command, when auditing is
    /// attached. Best-effort: storage failures are logged, never propagated.
    async fn audit_ack(&self, ack: &EaAck) {
        let Some(audit) = self.attached_audit() else {
            return;
        };
        let Some(record) = self.command(ack.id) else {
            return;
        };
        let kind = record.kind.as_str();
        match &record.state {
            CommandState::Pending => {}
            CommandState::Failed { reason } => {
                audit
                    .try_record(AuditEvent::new(
                        AuditKind::CommandFailed,
                        serde_json::json!({
                            "command_id": record.id.to_string(),
                            "kind": kind,
                            "error": reason
                        }),
                    ))
                    .await;
            }
            CommandState::Completed { payload } => {
                audit
                    .try_record(AuditEvent::new(
                        AuditKind::CommandCompleted,
                        serde_json::json!({
                            "command_id": record.id.to_string(),
                            "kind": kind,
                            "result": completed_summary(payload)
                        }),
                    ))
                    .await;
                if let CommandPayload::AccountSnapshot(snapshot) = payload {
                    // A managed position that vanished from the book was closed
                    // at the venue; journal it with its last observed values.
                    if let Some(previous) = self.take_previous_account() {
                        for closed in closed_managed_positions(&previous, snapshot) {
                            audit
                                .try_record(AuditEvent::new(
                                    AuditKind::PositionClosed,
                                    serde_json::json!({
                                        "ticket": closed.ticket,
                                        "symbol": closed.symbol,
                                        "kind": closed.kind,
                                        "lots": closed.lots,
                                        "price": closed.price,
                                        "profit": closed.profit
                                    }),
                                ))
                                .await;
                        }
                    }
                    audit
                        .try_record(AuditEvent::new(
                            AuditKind::BrokerSnapshot,
                            serde_json::json!({
                                "orders": snapshot.orders,
                                "lots": snapshot.lots,
                                "positions": snapshot.positions.len(),
                                "positionsTruncated": snapshot.positions_truncated
                            }),
                        ))
                        .await;
                    if let Some(summary) = crate::reconciliation::drift_summary(snapshot) {
                        tracing::warn!(%summary, "reconciliation drift detected");
                        audit
                            .try_record(AuditEvent::new(AuditKind::ReconciliationDrift, summary))
                            .await;
                    }
                }
            }
        }
    }

    /// Retains a validated snapshot as if its acknowledgement had completed.
    ///
    /// Test-only hatch so module tests can build account state without the
    /// poll round trip; production state always flows through [`Self::apply_ack`].
    #[cfg(test)]
    pub(crate) fn retain_snapshot(&self, payload: AccountSnapshotPayload) {
        self.with_last_account(|slot| {
            *slot = Some(StoredAccount {
                payload,
                at: SystemTime::now(),
            });
        });
    }

    /// Latest validated `account_snapshot` acknowledgement, if any.
    ///
    /// `None` until the first snapshot command completes. Read-only callers
    /// (risk facts, reconciliation) use it instead of querying the terminal.
    pub fn last_account(&self) -> Option<AccountSnapshotPayload> {
        self.with_last_account(|slot| slot.as_ref().map(|stored| stored.payload.clone()))
    }

    /// Age of the retained account snapshot, when one exists.
    pub fn last_account_age(&self, now: SystemTime) -> Option<Duration> {
        self.with_last_account(|slot| {
            slot.as_ref()
                .map(|stored| now.duration_since(stored.at).unwrap_or_default())
        })
    }

    /// Whether a command of this kind is still awaiting delivery or ack.
    pub fn has_pending(&self, kind: CommandKind) -> bool {
        self.with_commands(|queue| {
            queue.iter().any(|command| {
                command.kind == kind && matches!(command.state, CommandState::Pending)
            })
        })
    }

    fn with_last_account<T>(&self, apply: impl FnOnce(&mut Option<StoredAccount>) -> T) -> T {
        // Same poisoning stance as the other guards: the slot holds one whole
        // value, so recovering the guard cannot observe a partial write.
        let mut guard = match self.last_account.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply(&mut guard)
    }

    fn with_previous_account<T>(&self, apply: impl FnOnce(&mut Option<StoredAccount>) -> T) -> T {
        let mut guard = match self.previous_account.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply(&mut guard)
    }

    /// Consumes the snapshot replaced by the most recent one; used once to
    /// journal positions that disappeared from the book.
    fn take_previous_account(&self) -> Option<AccountSnapshotPayload> {
        self.with_previous_account(|slot| slot.take().map(|stored| stored.payload))
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

    fn enqueue_account_snapshot(&self) -> CommandId {
        self.enqueue(CommandKind::AccountSnapshot)
    }

    fn enqueue_order_check(&self, request: OrderRequest) -> CommandId {
        EaLink::enqueue_order_check(self, request)
    }

    fn enqueue_open_order(&self, request: OrderRequest) -> CommandId {
        EaLink::enqueue_order(self, request)
    }

    fn enqueue_close_order(&self, request: CloseOrderRequest) -> CommandId {
        EaLink::enqueue_close(self, request)
    }

    fn enqueue_modify_order(&self, request: ModifyOrderRequest) -> CommandId {
        EaLink::enqueue_modify(self, request)
    }

    fn enqueue_rates(&self, request: RatesRequest) -> CommandId {
        EaLink::enqueue_rates(self, request)
    }

    fn enqueue_symbol_spec(&self, request: SymbolSpecRequest) -> CommandId {
        EaLink::enqueue_symbol_spec(self, request)
    }

    fn enqueue_order_history(&self, request: OrderHistoryRequest) -> CommandId {
        EaLink::enqueue_order_history(self, request)
    }

    fn has_pending(&self, kind: CommandKind) -> bool {
        EaLink::has_pending(self, kind)
    }

    fn recent_commands(&self, limit: usize) -> Vec<ListedCommand> {
        EaLink::recent_commands(self, limit)
    }

    fn command(&self, id: CommandId) -> Option<CommandRecord> {
        EaLink::command(self, id)
    }

    async fn await_command(&self, id: CommandId, timeout: Duration) -> CommandState {
        EaLink::await_command(self, id, timeout).await
    }

    fn last_account(&self) -> Option<AccountSnapshotPayload> {
        EaLink::last_account(self)
    }

    fn last_account_age(&self, now: SystemTime) -> Option<Duration> {
        EaLink::last_account_age(self, now)
    }

    fn attach_audit(&self, audit: Arc<AuditRuntime>) {
        EaLink::set_audit(self, audit)
    }
}
#[post("/ea/poll")]
/// Accepts one EA poll after authenticating the shared token.
///
/// The body is read as raw bytes and parsed here because the MQL4 WebRequest
/// client cannot set a JSON content type; requiring one would reject every
/// real EA poll.
#[tracing::instrument(skip_all, name = "ea.poll")]
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
        link.audit_ack(&ack).await;
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
                    Some((id, kind, request)) => {
                        let (order, close, modify, rates, spec, history) = match request {
                            Some(CommandRequest::Order(order)) => {
                                (Some(Box::new(order)), None, None, None, None, None)
                            }
                            Some(CommandRequest::Close(close)) => {
                                (None, Some(Box::new(close)), None, None, None, None)
                            }
                            Some(CommandRequest::Modify(modify)) => {
                                (None, None, Some(Box::new(modify)), None, None, None)
                            }
                            Some(CommandRequest::Rates(rates)) => {
                                (None, None, None, Some(Box::new(rates)), None, None)
                            }
                            Some(CommandRequest::SymbolSpec(spec)) => {
                                (None, None, None, None, Some(Box::new(spec)), None)
                            }
                            Some(CommandRequest::OrderHistory(history)) => {
                                (None, None, None, None, None, Some(Box::new(history)))
                            }
                            None => (None, None, None, None, None, None),
                        };
                        EaReply::Command {
                            id,
                            kind,
                            order,
                            close,
                            modify,
                            rates,
                            spec,
                            history,
                        }
                    }
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
        CommandKind::OpenOrder => {
            let value = data.ok_or_else(|| "open_order ack is missing data".to_owned())?;
            let payload: OrderExecutionPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid open_order payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::OpenOrder(payload))
        }
        CommandKind::CloseOrder => {
            let value = data.ok_or_else(|| "close_order ack is missing data".to_owned())?;
            let payload: OrderExecutionPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid close_order payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::CloseOrder(payload))
        }
        CommandKind::ModifyOrder => {
            let value = data.ok_or_else(|| "modify_order ack is missing data".to_owned())?;
            let payload: OrderExecutionPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid modify_order payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::ModifyOrder(payload))
        }
        CommandKind::Rates => {
            let value = data.ok_or_else(|| "rates ack is missing data".to_owned())?;
            let payload: RatesPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid rates payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::Rates(payload))
        }
        CommandKind::SymbolSpec => {
            let value = data.ok_or_else(|| "symbol_spec ack is missing data".to_owned())?;
            let payload: SymbolSpecPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid symbol_spec payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::SymbolSpec(payload))
        }
        CommandKind::OrderHistory => {
            let value = data.ok_or_else(|| "order_history ack is missing data".to_owned())?;
            let payload: OrderHistoryPayload = serde_json::from_value(value)
                .map_err(|error| format!("invalid order_history payload: {error}"))?;
            payload.validate()?;
            Ok(CommandPayload::OrderHistory(payload))
        }
    }
}

/// Managed positions present in `previous` but missing from `current`.
///
/// Only meaningful with two complete book views: any truncated list makes the
/// comparison untrustworthy, so those snapshots yield no closures rather than
/// false journal entries.
fn closed_managed_positions(
    previous: &AccountSnapshotPayload,
    current: &AccountSnapshotPayload,
) -> Vec<PositionPayload> {
    if previous.positions_truncated || current.positions_truncated {
        return Vec::new();
    }
    previous
        .positions
        .iter()
        .filter(|position| position.magic == ORDER_MAGIC)
        .filter(|position| {
            !current
                .positions
                .iter()
                .any(|candidate| candidate.ticket == position.ticket)
        })
        .cloned()
        .collect()
}

/// Bounded audit summary of a completed command payload.
fn completed_summary(payload: &CommandPayload) -> Value {
    match payload {
        CommandPayload::Ping => serde_json::json!({}),
        CommandPayload::AccountSnapshot(snapshot) => serde_json::json!({
            "orders": snapshot.orders,
            "lots": snapshot.lots
        }),
        CommandPayload::OrderCheck(check) => serde_json::json!({
            "passed": check.passed,
            "retcode": check.retcode,
            "margin": check.margin
        }),
        CommandPayload::OpenOrder(execution)
        | CommandPayload::CloseOrder(execution)
        | CommandPayload::ModifyOrder(execution) => serde_json::json!({
            "executed": execution.executed,
            "retcode": execution.retcode,
            "ticket": execution.ticket
        }),
        CommandPayload::Rates(rates) => serde_json::json!({
            "symbol": rates.symbol,
            "timeframeMinutes": rates.timeframe_minutes,
            "candles": rates.candles.len()
        }),
        CommandPayload::SymbolSpec(spec) => serde_json::json!({
            "symbol": spec.symbol,
            "spreadPoints": spec.spread_points,
            "stopLevelPoints": spec.stop_level_points,
            "marginRequired": spec.margin_required
        }),
        CommandPayload::OrderHistory(history) => serde_json::json!({
            "orders": history.orders.len(),
            "total": history.total,
            "truncated": history.truncated
        }),
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
    tracing::info!(%address, "EA control channel listening");
    Ok(server)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        CloseOrderRequest, CommandRequest, EaAck, EaLink, EaReply, EaToken, ModifyOrderRequest,
        ORDER_MAGIC, OrderCheckPayload, OrderRequest, RatesPayload, RatesRequest,
        closed_managed_positions, hex_preview, payload_for,
    };
    use crate::broker::Symbol as BrokerSymbol;
    use crate::broker::{
        AccountSnapshotPayload, CandlePayload, CommandId, CommandKind, CommandPayload,
        CommandState, OrderHistoryRequest, PositionKind, PositionPayload, SymbolSpecRequest,
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
        assert_eq!(CommandKind::OpenOrder.as_str(), "open_order");
        assert_eq!(CommandKind::CloseOrder.as_str(), "close_order");
        assert_eq!(CommandKind::ModifyOrder.as_str(), "modify_order");
        assert_eq!(CommandKind::Rates.as_str(), "rates");
        assert_eq!(CommandKind::SymbolSpec.as_str(), "symbol_spec");
    }

    fn candle(time: i64) -> CandlePayload {
        CandlePayload {
            time,
            open: 1.1,
            high: 1.2,
            low: 1.0,
            close: 1.15,
            volume: 42,
        }
    }

    #[test]
    fn rates_requests_validate_timeframes_and_bounds() {
        let symbol = BrokerSymbol::parse("EURUSD").expect("symbol");
        let request = RatesRequest::new(&symbol, 240, 48).expect("valid request");
        assert_eq!(request.symbol(), "EURUSD");
        assert_eq!(request.timeframe_minutes(), 240);
        assert_eq!(request.bars(), 48);
        assert_eq!(
            serde_json::to_value(&request).expect("serializes"),
            serde_json::json!({"symbol": "EURUSD", "timeframeMinutes": 240, "bars": 48})
        );
        for minutes in [0, 7, 90, 43_201] {
            assert!(
                RatesRequest::new(&symbol, minutes, 10).is_err(),
                "must reject timeframe {minutes}"
            );
        }
        for bars in [0, 241] {
            assert!(
                RatesRequest::new(&symbol, 240, bars).is_err(),
                "must reject bars {bars}"
            );
        }
        assert!(
            RatesRequest::new(&symbol, 1, 240).is_ok(),
            "the full M1 window is valid"
        );
    }

    #[test]
    fn rates_payloads_require_monotonic_sane_candles() {
        let valid = RatesPayload {
            symbol: "EURUSD".to_owned(),
            timeframe_minutes: 240,
            candles: vec![candle(1_700_000_000), candle(1_700_014_400)],
        };
        valid.validate().expect("valid series");

        let broken = |mutate: &dyn Fn(&mut RatesPayload)| {
            let mut payload = valid.clone();
            mutate(&mut payload);
            payload
        };

        assert!(
            broken(&|p| p.symbol = "no spaces".to_owned())
                .validate()
                .is_err()
        );
        assert!(broken(&|p| p.timeframe_minutes = 90).validate().is_err());
        assert!(broken(&|p| p.candles.clear()).validate().is_err());
        assert!(
            broken(&|p| p.candles = (0..241)
                .map(|index| candle(1_700_000_000 + index))
                .collect())
            .validate()
            .is_err()
        );
        assert!(broken(&|p| p.candles.swap(0, 1)).validate().is_err());
        assert!(
            broken(&|p| p.candles[1].time = p.candles[0].time)
                .validate()
                .is_err(),
            "duplicate bar times are rejected"
        );
        assert!(
            broken(&|p| p.candles[0].high = f64::NAN)
                .validate()
                .is_err()
        );
        assert!(broken(&|p| p.candles[0].high = 1.0).validate().is_err());
        assert!(broken(&|p| p.candles[0].volume = -1).validate().is_err());
    }

    #[test]
    fn rates_commands_deliver_and_serialize() {
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(15),
        );
        let symbol = BrokerSymbol::parse("EURUSD").expect("symbol");
        let request = RatesRequest::new(&symbol, 240, 2).expect("request");
        let id = link.enqueue_rates(request.clone());

        let (delivered, kind, payload) = link.deliverable().expect("pending command");
        assert_eq!(delivered, id);
        assert_eq!(kind, CommandKind::Rates);
        assert_eq!(payload, Some(CommandRequest::Rates(request.clone())));

        let reply = EaReply::Command {
            id,
            kind,
            order: None,
            close: None,
            modify: None,
            rates: Some(Box::new(request)),
            spec: None,
            history: None,
        };
        let wire = serde_json::to_value(&reply).expect("serializes");
        assert_eq!(wire["t"], "cmd");
        assert_eq!(wire["kind"], "rates");
        assert_eq!(wire["rates"]["timeframeMinutes"], 240);
        assert_eq!(wire["rates"]["bars"], 2);
        assert!(wire.get("order").is_none(), "absent requests are omitted");
    }

    #[actix_web::test]
    async fn rates_acks_complete_commands_and_await_observes() {
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(15),
        );
        let symbol = BrokerSymbol::parse("EURUSD").expect("symbol");
        let id = link.enqueue_rates(RatesRequest::new(&symbol, 240, 2).expect("request"));
        assert_eq!(
            link.command(id).expect("record").state,
            CommandState::Pending
        );

        link.apply_ack(&EaAck {
            id,
            ok: true,
            data: Some(serde_json::json!({
                "symbol": "EURUSD",
                "timeframeMinutes": 240,
                "candles": [
                    {"time": 1_700_000_000, "open": 1.1, "high": 1.2, "low": 1.0, "close": 1.15, "volume": 42},
                    {"time": 1_700_014_400, "open": 1.15, "high": 1.3, "low": 1.1, "close": 1.25, "volume": 77}
                ]
            })),
            error: None,
        });
        match link.await_command(id, Duration::from_secs(1)).await {
            CommandState::Completed {
                payload: CommandPayload::Rates(rates),
            } => {
                assert_eq!(rates.symbol, "EURUSD");
                assert_eq!(rates.candles.len(), 2);
                assert_eq!(rates.candles[1].close, 1.25);
            }
            other => panic!("unexpected state: {other:?}"),
        }

        // A malformed series fails the command instead of completing it.
        let malformed = link.enqueue_rates(RatesRequest::new(&symbol, 240, 1).expect("request"));
        link.apply_ack(&EaAck {
            id: malformed,
            ok: true,
            data: Some(serde_json::json!({
                "symbol": "EURUSD",
                "timeframeMinutes": 240,
                "candles": [{"time": 0, "open": 1.1, "high": 1.2, "low": 1.0, "close": 1.15, "volume": 1}]
            })),
            error: None,
        });
        match link.await_command(malformed, Duration::from_secs(1)).await {
            CommandState::Failed { reason } => {
                assert!(
                    reason.contains("candle time"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("unexpected state: {other:?}"),
        }

        // A command that never gets acknowledged fails on the caller's deadline.
        let stalled = link.enqueue_rates(RatesRequest::new(&symbol, 240, 1).expect("request"));
        match link
            .await_command(stalled, Duration::from_millis(150))
            .await
        {
            CommandState::Failed { reason } => assert_eq!(reason, "await timeout"),
            other => panic!("unexpected state: {other:?}"),
        }

        // Unknown ids are observed as gone immediately.
        match link
            .await_command(CommandId::new(), Duration::from_millis(150))
            .await
        {
            CommandState::Failed { reason } => {
                assert_eq!(reason, "command left the retained history");
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[test]
    fn recent_commands_list_lifecycle_and_summaries() {
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(15),
        );
        let pending = link.enqueue(CommandKind::Ping);
        let completed = link.enqueue(CommandKind::AccountSnapshot);
        link.apply_ack(&EaAck {
            id: completed,
            ok: true,
            data: Some(serde_json::json!({
                "balance": 20.57,
                "equity": 20.57,
                "freeMargin": 20.57,
                "orders": 1,
                "lots": 0.01,
                "positions": [],
                "positionsTruncated": false,
                "serverTime": 1_758_000_000
            })),
            error: None,
        });
        let failed = link.enqueue(CommandKind::Ping);
        link.apply_ack(&EaAck {
            id: failed,
            ok: false,
            data: None,
            error: Some("terminal busy".to_owned()),
        });

        let listed = link.recent_commands(10);
        assert_eq!(listed.len(), 3, "newest first");
        assert_eq!(listed[0].id, failed);
        assert_eq!(listed[0].status, "failed");
        assert_eq!(listed[0].reason.as_deref(), Some("terminal busy"));
        assert_eq!(listed[1].id, completed);
        assert_eq!(listed[1].status, "completed");
        let summary = listed[1].summary.as_ref().expect("summary");
        assert_eq!(summary["orders"], 1);
        assert_eq!(summary["lots"], 0.01);
        assert_eq!(listed[2].id, pending);
        assert_eq!(listed[2].status, "pending");

        assert_eq!(link.recent_commands(1).len(), 1, "the cap applies");
    }

    fn book_position(ticket: i64, magic: u32) -> PositionPayload {
        PositionPayload {
            ticket,
            symbol: "EURUSD".to_owned(),
            kind: PositionKind::Buy,
            lots: 0.01,
            price: 1.095,
            profit: -0.42,
            stop_loss: 1.085,
            take_profit: 1.105,
            opened_at: 1_758_000_000,
            current: 1.096,
            swap: -0.11,
            commission: 0.0,
            magic,
        }
    }

    fn book(positions: Vec<PositionPayload>, truncated: bool) -> AccountSnapshotPayload {
        AccountSnapshotPayload {
            balance: 20.0,
            equity: 20.0,
            free_margin: 20.0,
            orders: positions.len() as u32,
            lots: positions.iter().map(|position| position.lots).sum(),
            positions,
            positions_truncated: truncated,
            server_time: 0,
            leverage: 100,
            margin_level: 0.0,
        }
    }

    #[test]
    fn closures_are_detected_only_with_complete_books() {
        let closed = closed_managed_positions(
            &book(
                vec![book_position(1, ORDER_MAGIC), book_position(2, 0)],
                false,
            ),
            &book(vec![book_position(2, 0)], false),
        );
        assert_eq!(closed.len(), 1, "only the vanished managed position counts");
        assert_eq!(closed[0].ticket, 1);
        assert_eq!(closed[0].profit, -0.42);

        assert!(
            closed_managed_positions(
                &book(vec![book_position(1, ORDER_MAGIC)], false),
                &book(vec![book_position(1, ORDER_MAGIC)], false)
            )
            .is_empty(),
            "an unchanged book closes nothing"
        );
        assert!(
            closed_managed_positions(
                &book(vec![book_position(1, 0)], false),
                &book(Vec::new(), false)
            )
            .is_empty(),
            "foreign positions are never journaled"
        );
        assert!(
            closed_managed_positions(
                &book(vec![book_position(1, ORDER_MAGIC)], false),
                &book(Vec::new(), true)
            )
            .is_empty(),
            "a truncated view could hide the ticket"
        );
        assert!(
            closed_managed_positions(
                &book(vec![book_position(1, ORDER_MAGIC)], true),
                &book(Vec::new(), false)
            )
            .is_empty(),
            "a truncated previous view is equally untrustworthy"
        );
    }

    #[test]
    fn position_stops_must_be_non_negative() {
        let snapshot = |stop_loss: f64, take_profit: f64| {
            let mut snapshot = book(vec![book_position(1, ORDER_MAGIC)], false);
            snapshot.positions[0].stop_loss = stop_loss;
            snapshot.positions[0].take_profit = take_profit;
            snapshot
        };
        snapshot(1.085, 1.105).validate().expect("valid stops");
        snapshot(0.0, 0.0)
            .validate()
            .expect("zero means no stop and is valid");
        assert!(snapshot(-0.5, 1.105).validate().is_err());
        assert!(snapshot(1.085, f64::NAN).validate().is_err());
    }

    #[actix_web::test]
    async fn vanished_managed_positions_are_journaled_once() {
        use crate::audit::{AuditKind, AuditRuntime, MemoryTrail};

        let trail = Arc::new(MemoryTrail::default());
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(15),
        );
        link.set_audit(Arc::new(AuditRuntime::new(trail.clone())));

        let snapshot_data = |positions: Vec<PositionPayload>| {
            serde_json::json!({
                "balance": 20.0,
                "equity": 20.0,
                "freeMargin": 20.0,
                "orders": positions.len(),
                "lots": 0.01,
                "positions": positions,
                "positionsTruncated": false,
                "serverTime": 1_758_000_000
            })
        };

        let first = link.enqueue(CommandKind::AccountSnapshot);
        let ack = EaAck {
            id: first,
            ok: true,
            data: Some(snapshot_data(vec![book_position(777, ORDER_MAGIC)])),
            error: None,
        };
        link.apply_ack(&ack);
        link.audit_ack(&ack).await;

        let second = link.enqueue(CommandKind::AccountSnapshot);
        let ack = EaAck {
            id: second,
            ok: true,
            data: Some(snapshot_data(Vec::new())),
            error: None,
        };
        link.apply_ack(&ack);
        link.audit_ack(&ack).await;

        let closed: Vec<_> = trail
            .events()
            .into_iter()
            .filter(|event| event.kind() == AuditKind::PositionClosed)
            .collect();
        assert_eq!(closed.len(), 1, "the vanished position is journaled");
        assert_eq!(closed[0].payload()["ticket"], 777);
        assert_eq!(closed[0].payload()["profit"], -0.42);
        assert_eq!(closed[0].payload()["lots"], 0.01);

        // A further unchanged snapshot must not repeat the journal entry.
        let third = link.enqueue(CommandKind::AccountSnapshot);
        let ack = EaAck {
            id: third,
            ok: true,
            data: Some(snapshot_data(Vec::new())),
            error: None,
        };
        link.apply_ack(&ack);
        link.audit_ack(&ack).await;
        assert_eq!(
            trail
                .events()
                .iter()
                .filter(|event| event.kind() == AuditKind::PositionClosed)
                .count(),
            1
        );
    }

    #[test]
    fn heartbeat_live_orders_flag_reports_armed_state() {
        let parse = |live_orders: Option<bool>| {
            let mut body = serde_json::json!({
                "t": "hb",
                "token": "test-token-1234567890",
                "acct": 94168,
                "server": "IFCMarkets-Real",
                "symbol": "EURUSD",
                "connected": true,
                "tradeAllowed": true,
                "orders": 0,
                "lots": 0.0
            });
            if let Some(armed) = live_orders {
                body["liveOrders"] = serde_json::json!(armed);
            }
            serde_json::from_value::<super::EaPoll>(body)
                .expect("poll parses")
                .snapshot()
                .expect("snapshot validates")
        };
        assert!(parse(Some(true)).live_orders(), "armed EA reports armed");
        assert!(!parse(Some(false)).live_orders());
        assert!(
            !parse(None).live_orders(),
            "an older EA without the field is treated as disarmed"
        );
    }

    #[test]
    fn non_finite_money_is_rejected() {
        let payload = AccountSnapshotPayload {
            balance: f64::NAN,
            equity: 0.0,
            free_margin: 0.0,
            orders: 0,
            lots: 0.0,
            positions: Vec::new(),
            positions_truncated: false,
            server_time: 0,
            leverage: 100,
            margin_level: 0.0,
        };
        let error = payload.validate().expect_err("NaN must be rejected");
        assert!(error.contains("balance"), "unexpected error: {error}");
    }

    #[test]
    fn snapshot_payloads_reject_unusable_exposure() {
        let base = || {
            serde_json::json!({
                "balance": 20.57,
                "equity": 20.57,
                "freeMargin": 20.57,
                "orders": 1,
                "lots": 0.01,
                "positions": [{
                    "ticket": 123,
                    "symbol": "EURUSD",
                    "kind": "buy",
                    "lots": 0.01,
                    "magic": 77041,
                    "price": 1.095,
                    "profit": -0.25
                }],
                "positionsTruncated": false,
                "serverTime": 1_758_000_000
            })
        };

        let valid = payload_for(CommandKind::AccountSnapshot, Some(base()))
            .expect("complete payload must validate");
        assert!(matches!(valid, CommandPayload::AccountSnapshot(_)));

        let mut cases = Vec::new();
        let mut negative_lots = base();
        negative_lots["lots"] = serde_json::json!(-1.0);
        cases.push(negative_lots);

        let mut bad_ticket = base();
        bad_ticket["positions"][0]["ticket"] = serde_json::json!(0);
        cases.push(bad_ticket);

        let mut bad_symbol = base();
        bad_symbol["positions"][0]["symbol"] = serde_json::json!("not a symbol!");
        cases.push(bad_symbol);

        let mut bad_lots = base();
        bad_lots["positions"][0]["lots"] = serde_json::json!(0.0);
        cases.push(bad_lots);

        let mut bad_price = base();
        bad_price["positions"][0]["price"] = serde_json::json!(-1.0);
        cases.push(bad_price);

        let mut bad_profit = base();
        bad_profit["positions"][0]["profit"] = serde_json::json!("lots");
        cases.push(bad_profit);

        let mut too_many = base();
        let entries = (0..65)
            .map(|ticket| {
                serde_json::json!({
                    "ticket": ticket + 1,
                    "symbol": "EURUSD",
                    "kind": "sell",
                    "lots": 0.01,
                    "magic": 77041,
                    "price": 1.1,
                    "profit": 0.0
                })
            })
            .collect::<Vec<_>>();
        too_many["positions"] = serde_json::json!(entries);
        cases.push(too_many);

        for case in cases {
            assert!(
                payload_for(CommandKind::AccountSnapshot, Some(case)).is_err(),
                "payload must be rejected"
            );
        }

        let mut unknown_kind = base();
        unknown_kind["positions"][0]["kind"] = serde_json::json!("sideways");
        assert!(payload_for(CommandKind::AccountSnapshot, Some(unknown_kind)).is_err());
    }

    #[test]
    fn snapshot_validation_rejects_unusable_money_and_positions() {
        let position = || PositionPayload {
            ticket: 1,
            symbol: "EURUSD".to_owned(),
            magic: ORDER_MAGIC,
            kind: PositionKind::Buy,
            lots: 0.01,
            price: 1.1,
            profit: 0.0,
            stop_loss: 0.0,
            take_profit: 0.0,
            opened_at: 1_700_000_000,
            current: 1.1,
            swap: 0.0,
            commission: 0.0,
        };
        let base = || AccountSnapshotPayload {
            balance: 1.0,
            equity: 1.0,
            free_margin: 1.0,
            orders: 1,
            lots: 0.01,
            positions: vec![position()],
            positions_truncated: false,
            server_time: 1_700_000_000,
            leverage: 100,
            margin_level: 0.0,
        };

        let mut negative_margin_level = base();
        negative_margin_level.margin_level = -1.0;
        assert!(
            negative_margin_level
                .validate()
                .expect_err("negative margin level")
                .contains("marginLevel")
        );

        for (name, mutate) in [
            (
                "profit",
                Box::new(|payload: &mut AccountSnapshotPayload| {
                    payload.positions[0].profit = f64::NAN;
                }) as Box<dyn Fn(&mut AccountSnapshotPayload)>,
            ),
            (
                "swap",
                Box::new(|payload: &mut AccountSnapshotPayload| {
                    payload.positions[0].swap = f64::INFINITY;
                }),
            ),
            (
                "commission",
                Box::new(|payload: &mut AccountSnapshotPayload| {
                    payload.positions[0].commission = f64::NAN;
                }),
            ),
            (
                "sl",
                Box::new(|payload: &mut AccountSnapshotPayload| {
                    payload.positions[0].stop_loss = -1.0;
                }),
            ),
            (
                "openedAt",
                Box::new(|payload: &mut AccountSnapshotPayload| {
                    payload.positions[0].opened_at = -1;
                }),
            ),
            (
                "current",
                Box::new(|payload: &mut AccountSnapshotPayload| {
                    payload.positions[0].current = -1.0;
                }),
            ),
        ] {
            let mut broken = base();
            mutate(&mut broken);
            let error = broken.validate().expect_err(name);
            assert!(error.contains(name), "{name}: {error}");
        }
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
        let request = OrderRequest::from_intent(&intent);
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
        let wire = serde_json::to_value(OrderRequest::from_intent(&TradeIntent::approve(limit)))
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
        let id = link.enqueue_order_check(OrderRequest::from_intent(&intent));
        let (delivered_id, kind, order) = link.deliverable().expect("pending command");
        assert_eq!(delivered_id, id);
        assert_eq!(kind, CommandKind::OrderCheck);
        assert_eq!(
            order,
            Some(CommandRequest::Order(OrderRequest::from_intent(&intent)))
        );

        let record = link.command(id).expect("record");
        assert_eq!(record.state, CommandState::Pending);
    }

    #[test]
    fn open_order_payloads_require_a_consistent_verdict() {
        let dry_run = payload_for(
            CommandKind::OpenOrder,
            Some(serde_json::json!({
                "executed": false,
                "retcode": 0,
                "comment": "dry run (live orders disabled in EA)",
                "ticket": 0,
                "price": 0.0
            })),
        )
        .expect("dry-run payload must validate");
        assert!(matches!(dry_run, CommandPayload::OpenOrder(_)));

        let executed = payload_for(
            CommandKind::OpenOrder,
            Some(serde_json::json!({
                "executed": true,
                "retcode": 10009,
                "comment": "done",
                "ticket": 123456,
                "price": 1.095
            })),
        )
        .expect("executed payload must validate");
        assert!(matches!(executed, CommandPayload::OpenOrder(_)));

        let cases = [
            serde_json::json!({"executed": true, "retcode": 10009, "comment": "done", "ticket": 0, "price": 1.095}),
            serde_json::json!({"executed": false, "retcode": 0, "comment": "dry run", "ticket": -1, "price": 0.0}),
            serde_json::json!({"executed": false, "retcode": 0, "comment": "dry run", "ticket": 0, "price": -1.0}),
        ];
        for case in cases {
            assert!(
                payload_for(CommandKind::OpenOrder, Some(case)).is_err(),
                "inconsistent verdict must be rejected"
            );
        }
        assert!(payload_for(CommandKind::OpenOrder, None).is_err());
    }

    #[test]
    fn open_orders_carry_the_vevra_magic_to_the_terminal() {
        let intent = TradeIntent::approve(TradeIntentDraft::new(
            parse_instrument("EURUSD").expect("symbol"),
            Side::Buy,
            OrderKind::Market,
            Volume::parse(0.01).expect("volume"),
            None,
            None,
            None,
        ));
        let request = OrderRequest::from_intent(&intent);
        let wire = serde_json::to_value(&request).expect("serializable");
        assert_eq!(wire["magic"], ORDER_MAGIC);

        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        let id = link.enqueue_order(request.clone());
        let (delivered_id, kind, order) = link.deliverable().expect("pending command");
        assert_eq!(delivered_id, id);
        assert_eq!(kind, CommandKind::OpenOrder);
        assert_eq!(order, Some(CommandRequest::Order(request)));
    }

    #[test]
    fn close_orders_carry_the_ticket_and_magic() {
        let request = CloseOrderRequest::new(123, ORDER_MAGIC);
        let wire = serde_json::to_value(&request).expect("serializable");
        assert_eq!(wire["ticket"], 123);
        assert_eq!(wire["magic"], ORDER_MAGIC);
        assert_eq!(request.ticket(), 123);
        assert_eq!(request.magic(), ORDER_MAGIC);

        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        let id = link.enqueue_close(request.clone());
        let (delivered_id, kind, payload) = link.deliverable().expect("pending command");
        assert_eq!(delivered_id, id);
        assert_eq!(kind, CommandKind::CloseOrder);
        assert_eq!(payload, Some(CommandRequest::Close(request)));
    }

    #[test]
    fn close_payloads_require_a_consistent_verdict() {
        let dry_run = payload_for(
            CommandKind::CloseOrder,
            Some(serde_json::json!({
                "executed": false,
                "retcode": 0,
                "comment": "dry run (live orders disabled in EA)",
                "ticket": 0,
                "price": 0.0
            })),
        )
        .expect("dry-run close payload must validate");
        assert!(matches!(dry_run, CommandPayload::CloseOrder(_)));

        let closed = payload_for(
            CommandKind::CloseOrder,
            Some(serde_json::json!({
                "executed": true,
                "retcode": 0,
                "comment": "closed",
                "ticket": 123,
                "price": 1.095
            })),
        )
        .expect("closed payload must validate");
        assert!(matches!(closed, CommandPayload::CloseOrder(_)));

        assert!(payload_for(CommandKind::CloseOrder, None).is_err());
        assert!(
            payload_for(
                CommandKind::CloseOrder,
                Some(serde_json::json!({
                    "executed": true,
                    "retcode": 0,
                    "comment": "closed",
                    "ticket": 0,
                    "price": 1.095
                })),
            )
            .is_err(),
            "an executed close must report its ticket"
        );
    }

    #[test]
    fn modify_orders_carry_their_stops_and_magic() {
        let request = ModifyOrderRequest::new(123, ORDER_MAGIC, Some(1.05), None);
        let wire = serde_json::to_value(&request).expect("serializable");
        assert_eq!(wire["ticket"], 123);
        assert_eq!(wire["magic"], ORDER_MAGIC);
        assert_eq!(wire["stop_loss"], 1.05);
        assert!(
            wire.get("take_profit").is_none(),
            "absent stops are omitted"
        );
        assert_eq!(request.ticket(), 123);
        assert_eq!(request.magic(), ORDER_MAGIC);
        assert_eq!(request.stop_loss(), Some(1.05));
        assert_eq!(request.take_profit(), None);

        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        let id = link.enqueue_modify(request.clone());
        let (delivered_id, kind, payload) = link.deliverable().expect("pending command");
        assert_eq!(delivered_id, id);
        assert_eq!(kind, CommandKind::ModifyOrder);
        assert_eq!(payload, Some(CommandRequest::Modify(request)));
    }

    #[test]
    fn modify_payloads_require_a_consistent_verdict() {
        let dry_run = payload_for(
            CommandKind::ModifyOrder,
            Some(serde_json::json!({
                "executed": false,
                "retcode": 0,
                "comment": "dry run (live orders disabled in EA)",
                "ticket": 0,
                "price": 0.0
            })),
        )
        .expect("dry-run modify payload must validate");
        assert!(matches!(dry_run, CommandPayload::ModifyOrder(_)));
        assert!(payload_for(CommandKind::ModifyOrder, None).is_err());
        assert!(
            payload_for(
                CommandKind::ModifyOrder,
                Some(serde_json::json!({
                    "executed": true,
                    "retcode": 0,
                    "comment": "stops changed",
                    "ticket": 0,
                    "price": 1.1
                })),
            )
            .is_err(),
            "an executed modify must report its ticket"
        );
    }

    fn spec_json() -> serde_json::Value {
        serde_json::json!({
            "symbol": "EURUSD",
            "digits": 5,
            "point": 0.00001,
            "spreadPoints": 12,
            "stopLevelPoints": 5,
            "freezeLevelPoints": 0,
            "lotMin": 0.01,
            "lotMax": 100.0,
            "lotStep": 0.01,
            "tickValue": 0.1,
            "tickSize": 0.00001,
            "marginRequired": 3.29,
            "swapLong": -0.72,
            "swapShort": -0.31,
            "swapType": 0,
            "tradeAllowed": true
        })
    }

    #[test]
    fn symbol_specs_deliver_validate_and_summarize() {
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        let symbol = BrokerSymbol::parse("EURUSD").expect("symbol");
        let request = SymbolSpecRequest::new(&symbol);
        let id = link.enqueue_symbol_spec(request.clone());

        let (delivered, kind, payload) = link.deliverable().expect("pending command");
        assert_eq!(delivered, id);
        assert_eq!(kind, CommandKind::SymbolSpec);
        assert_eq!(payload, Some(CommandRequest::SymbolSpec(request.clone())));

        let reply = EaReply::Command {
            id,
            kind,
            order: None,
            close: None,
            modify: None,
            rates: None,
            spec: Some(Box::new(request)),
            history: None,
        };
        let wire = serde_json::to_value(&reply).expect("serializes");
        assert_eq!(wire["kind"], "symbol_spec");
        assert_eq!(wire["spec"]["symbol"], "EURUSD");
        assert!(wire.get("rates").is_none(), "absent requests are omitted");

        link.apply_ack(&EaAck {
            id,
            ok: true,
            data: Some(spec_json()),
            error: None,
        });
        let record = link.command(id).expect("record");
        let CommandState::Completed {
            payload: CommandPayload::SymbolSpec(spec),
        } = record.state
        else {
            panic!("expected a completed symbol spec");
        };
        assert_eq!(spec.symbol, "EURUSD");
        let listed = link.recent_commands(1);
        let summary = listed[0].summary.as_ref().expect("summary");
        assert_eq!(summary["spreadPoints"], 12);
        assert_eq!(summary["marginRequired"], 3.29);

        // A malformed acknowledgement fails the command instead of storing junk.
        let bad = link.enqueue_symbol_spec(SymbolSpecRequest::new(&symbol));
        link.apply_ack(&EaAck {
            id: bad,
            ok: true,
            data: Some(serde_json::json!({"symbol": "EURUSD"})),
            error: None,
        });
        assert!(matches!(
            link.command(bad).expect("record").state,
            CommandState::Failed { .. }
        ));
    }

    fn history_json() -> serde_json::Value {
        serde_json::json!({
            "orders": [{
                "ticket": 10650830,
                "symbol": "USDJPY",
                "kind": "buy",
                "lots": 0.01,
                "openPrice": 156.198,
                "closePrice": 156.41,
                "openTime": 1_789_699_082_i64,
                "closeTime": 1_789_707_257_i64,
                "profit": 1.36,
                "swap": 0.0,
                "commission": 0.0,
                "magic": ORDER_MAGIC
            }],
            "total": 1,
            "truncated": false
        })
    }

    #[test]
    fn order_history_commands_deliver_validate_and_summarize() {
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        assert!(OrderHistoryRequest::new(0, ORDER_MAGIC).is_err());
        assert!(OrderHistoryRequest::new(366, ORDER_MAGIC).is_err());
        let request = OrderHistoryRequest::new(30, ORDER_MAGIC).expect("request");
        assert_eq!(request.days(), 30);
        assert_eq!(request.magic(), ORDER_MAGIC);
        let id = link.enqueue_order_history(request.clone());

        let (delivered, kind, payload) = link.deliverable().expect("pending command");
        assert_eq!(delivered, id);
        assert_eq!(kind, CommandKind::OrderHistory);
        assert_eq!(payload, Some(CommandRequest::OrderHistory(request.clone())));

        let reply = EaReply::Command {
            id,
            kind,
            order: None,
            close: None,
            modify: None,
            rates: None,
            spec: None,
            history: Some(Box::new(request)),
        };
        let wire = serde_json::to_value(&reply).expect("serializes");
        assert_eq!(wire["kind"], "order_history");
        assert_eq!(wire["history"]["days"], 30);
        assert_eq!(wire["history"]["magic"], ORDER_MAGIC);
        assert!(wire.get("spec").is_none(), "absent requests are omitted");

        link.apply_ack(&EaAck {
            id,
            ok: true,
            data: Some(history_json()),
            error: None,
        });
        let record = link.command(id).expect("record");
        let CommandState::Completed {
            payload: CommandPayload::OrderHistory(history),
        } = record.state
        else {
            panic!("expected a completed order history");
        };
        assert_eq!(history.orders.len(), 1);
        assert_eq!(history.orders[0].net_profit(), 1.36);
        assert!(matches!(history.orders[0].kind, PositionKind::Buy));
        let listed = link.recent_commands(1);
        let summary = listed[0].summary.as_ref().expect("summary");
        assert_eq!(summary["orders"], 1);
        assert_eq!(summary["truncated"], false);

        // A malformed acknowledgement fails the command instead of storing junk.
        let bad =
            link.enqueue_order_history(OrderHistoryRequest::new(7, ORDER_MAGIC).expect("request"));
        let mut broken = history_json();
        broken["orders"][0]["closeTime"] = serde_json::json!(1);
        link.apply_ack(&EaAck {
            id: bad,
            ok: true,
            data: Some(broken),
            error: None,
        });
        assert!(matches!(
            link.command(bad).expect("record").state,
            CommandState::Failed { .. }
        ));
    }

    #[actix_web::test]
    async fn acknowledged_commands_are_audited_when_a_trail_is_attached() {
        use crate::audit::{AuditKind, AuditRuntime, MemoryTrail};

        let trail = Arc::new(MemoryTrail::default());
        let link = EaLink::new(
            EaToken::parse("test-token-1234567890").expect("token"),
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        link.set_audit(Arc::new(AuditRuntime::new(trail.clone())));

        let id = link.enqueue(CommandKind::AccountSnapshot);
        let ack = EaAck {
            id,
            ok: true,
            data: Some(serde_json::json!({
                "balance": 20.57,
                "equity": 20.57,
                "freeMargin": 20.57,
                "orders": 1,
                "lots": 0.01,
                "positions": [{
                    "ticket": 123,
                    "symbol": "EURUSD",
                    "kind": "buy",
                    "lots": 0.01,
                    "magic": ORDER_MAGIC,
                    "price": 1.095,
                    "profit": -0.25
                }],
                "positionsTruncated": false,
                "serverTime": 1_758_000_000
            })),
            error: None,
        };
        link.apply_ack(&ack);
        link.audit_ack(&ack).await;

        let events = trail.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind(), AuditKind::CommandCompleted);
        assert_eq!(events[0].payload()["kind"], "account_snapshot");
        assert_eq!(events[1].kind(), AuditKind::BrokerSnapshot);
        assert_eq!(events[1].payload()["orders"], 1);

        let failing = link.enqueue(CommandKind::Ping);
        let failure = EaAck {
            id: failing,
            ok: false,
            data: None,
            error: Some("nope".to_owned()),
        };
        link.apply_ack(&failure);
        link.audit_ack(&failure).await;

        let events = trail.events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].kind(), AuditKind::CommandFailed);
        assert_eq!(events[2].payload()["error"], "nope");

        // A foreign position turns the next snapshot into audited drift.
        let drifting = link.enqueue(CommandKind::AccountSnapshot);
        let foreign_ack = EaAck {
            id: drifting,
            ok: true,
            data: Some(serde_json::json!({
                "balance": 20.57,
                "equity": 20.57,
                "freeMargin": 20.57,
                "orders": 1,
                "lots": 0.01,
                "positions": [{
                    "ticket": 456,
                    "symbol": "EURUSD",
                    "kind": "buy",
                    "lots": 0.01,
                    "magic": 0,
                    "price": 1.095,
                    "profit": -0.25
                }],
                "positionsTruncated": false,
                "serverTime": 1_758_000_000
            })),
            error: None,
        };
        link.apply_ack(&foreign_ack);
        link.audit_ack(&foreign_ack).await;

        let events = trail.events();
        assert_eq!(events.len(), 7);
        assert_eq!(events[3].kind(), AuditKind::CommandCompleted);
        // The managed ticket 123 vanished in this snapshot, so the journal
        // records the closure before the new book state.
        assert_eq!(events[4].kind(), AuditKind::PositionClosed);
        assert_eq!(events[4].payload()["ticket"], 123);
        assert_eq!(events[5].kind(), AuditKind::BrokerSnapshot);
        assert_eq!(events[6].kind(), AuditKind::ReconciliationDrift);
        assert_eq!(
            events[6].payload()["unknownTickets"],
            serde_json::json!([456])
        );
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
                "lots": 0.03,
                "positions": [{
                    "ticket": 123,
                    "symbol": "EURUSD",
                    "kind": "buy",
                    "lots": 0.03,
                    "magic": 77041,
                    "price": 1.095,
                    "profit": -0.25
                }],
                "positionsTruncated": false,
                "serverTime": 1_758_000_000
            })),
            error: None,
        });

        let retained = link.last_account().expect("snapshot retained");
        assert_eq!(retained.orders, 3);
        assert_eq!(retained.lots, 0.03);
        assert_eq!(retained.positions[0].kind, PositionKind::Buy);
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
