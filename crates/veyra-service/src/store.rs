//! PostgreSQL audit storage (SQLx).
//!
//! Thin glue between the audit boundary and the `audit_events` table: connect
//! with bounded pooling and timeouts, run the embedded migrations, append
//! events, read the newest rows, and answer filtered reads with one
//! parameterised statement that mirrors `AuditQuery::matches`. Behaviour
//! depends on a running server, so this module is exercised by
//! `tests/store_live.rs` and the smoke script rather than the deterministic
//! suite (see `.cargo/config.toml`); only the generated SQL text is checked
//! offline.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::audit::{AuditError, AuditEvent, AuditProvider, AuditQuery, AuditRow, AuditTrail};
use crate::balance::BalancePoint;
use crate::state::{StateError, StateStore};

/// PostgreSQL-backed audit trail.
#[derive(Debug)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects with a small bounded pool and a connect timeout.
    ///
    /// # Errors
    /// Returns [`AuditError::Storage`] when the server is unreachable or the
    /// connection string is invalid.
    pub async fn connect(url: &str) -> Result<Self, AuditError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(3))
            .connect(url)
            .await
            .map_err(|error| storage_error("connect", &error))?;
        Ok(Self { pool })
    }

    /// Runs the embedded migrations.
    ///
    /// # Errors
    /// Returns [`AuditError::Storage`] when a migration fails.
    pub async fn migrate(&self) -> Result<(), AuditError> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|error| storage_error("migrate", &error))
    }

    /// Appends one event.
    ///
    /// # Errors
    /// Returns [`AuditError::Storage`] when the insert fails.
    pub async fn append(&self, event: &AuditEvent) -> Result<(), AuditError> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("insert into audit_events (id, kind, payload) values ($1::uuid, $2, $3)")
            .bind(id)
            .bind(event.kind().as_str())
            .bind(event.payload())
            .execute(&self.pool)
            .await
            .map_err(|error| storage_error("insert", &error))?;
        Ok(())
    }

    /// Deletes rows older than `keep_days`, returning how many were removed.
    ///
    /// # Errors
    /// Returns [`AuditError::Storage`] when the delete fails.
    pub async fn delete_older_than(&self, keep_days: u32) -> Result<u64, AuditError> {
        if keep_days == 0 {
            return Ok(0);
        }
        let days = i32::try_from(keep_days).unwrap_or(i32::MAX);
        let result =
            sqlx::query("delete from audit_events where at < now() - make_interval(days => $1)")
                .bind(days)
                .execute(&self.pool)
                .await
                .map_err(|error| storage_error("prune", &error))?;
        Ok(result.rows_affected())
    }

    /// Returns the newest rows, newest first.
    ///
    /// # Errors
    /// Returns [`AuditError::Storage`] when the query fails.
    pub async fn list(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError> {
        let rows = sqlx::query(
            "select id::text as id, at::text as at, kind, payload \
             from audit_events order by at desc, id desc limit $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_error("select", &error))?;
        Ok(rows
            .into_iter()
            .map(|row| AuditRow {
                id: row.get("id"),
                at: row.get("at"),
                kind: row.get("kind"),
                payload: row.get::<Value, _>("payload"),
            })
            .collect())
    }
}

impl Store {
    /// Reads one runtime-state row.
    ///
    /// # Errors
    /// Returns [`StateError::Storage`] when the query fails.
    pub async fn load_state(&self, key: &str) -> Result<Option<Value>, StateError> {
        let row = sqlx::query("select value from runtime_state where key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| state_error("load", &error))?;
        Ok(row.map(|row| row.get::<Value, _>("value")))
    }

    /// Writes one runtime-state row, replacing any previous value.
    ///
    /// # Errors
    /// Returns [`StateError::Storage`] when the upsert fails.
    pub async fn save_state(&self, key: &str, value: &Value) -> Result<(), StateError> {
        sqlx::query(
            "insert into runtime_state (key, value) values ($1, $2) \
             on conflict (key) do update set value = excluded.value, updated_at = now()",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|error| state_error("save", &error))?;
        Ok(())
    }
}

#[async_trait]
impl StateStore for Store {
    async fn load(&self, key: &str) -> Result<Option<Value>, StateError> {
        self.load_state(key).await
    }

    async fn save(&self, key: &str, value: &Value) -> Result<(), StateError> {
        self.save_state(key, value).await
    }
}

#[async_trait]
impl AuditTrail for Store {
    fn provider(&self) -> AuditProvider {
        AuditProvider::Postgres
    }

    async fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        self.append(&event).await
    }

    async fn recent(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError> {
        self.list(limit).await
    }

    async fn recent_decisions(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError> {
        let rows = sqlx::query(
            "select id::text as id, at::text as at, kind, payload \
             from audit_events \
             where kind in ('proposal_evaluated', 'position_closed', 'command_failed') \
             order by at desc, id desc limit $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_error("select decisions", &error))?;
        Ok(rows
            .into_iter()
            .map(|row| AuditRow {
                id: row.get("id"),
                at: row.get("at"),
                kind: row.get("kind"),
                payload: row.get::<Value, _>("payload"),
            })
            .collect())
    }

    async fn query(&self, query: &AuditQuery) -> Result<Vec<AuditRow>, AuditError> {
        let rows = filtered_query(query)
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| storage_error("select filtered", &error))?;
        Ok(rows
            .into_iter()
            .map(|row| AuditRow {
                id: row.get("id"),
                at: row.get("at"),
                kind: row.get("kind"),
                payload: row.get::<Value, _>("payload"),
            })
            .collect())
    }

    async fn balance_history(
        &self,
        login: u64,
        server: &str,
        since_ms: u64,
    ) -> Result<Vec<BalancePoint>, AuditError> {
        let rows = sqlx::query(
            "select (payload->>'atMs')::bigint as at_ms, \
                    (payload->>'balance')::double precision as balance \
             from audit_events \
             where kind = 'balance_observed' \
               and payload->>'login' = $1 \
               and payload->>'server' = $2 \
               and (payload->>'atMs')::bigint >= $3 \
             order by (payload->>'atMs')::bigint asc, at asc, id asc",
        )
        .bind(login.to_string())
        .bind(server)
        .bind(i64::try_from(since_ms).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_error("balance history", &error))?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let at_ms = u64::try_from(row.get::<i64, _>("at_ms")).ok()?;
                let balance = row.get::<f64, _>("balance");
                balance
                    .is_finite()
                    .then_some(BalancePoint { at_ms, balance })
            })
            .collect())
    }

    async fn prune(&self, keep_days: u32) -> Result<u64, AuditError> {
        self.delete_older_than(keep_days).await
    }
}

/// Builds the one parameterised statement behind [`AuditTrail::query`].
///
/// Only validated values are bound (never spliced), and each optional filter
/// adds its clause only when present, so the planner sees a concrete shape.
/// The predicates mirror [`AuditQuery::matches`]: upper-cased symbol equality,
/// ticket on `payload.ticket` or `payload.result.ticket`, command-id set
/// membership, exact outcome, and the half-open `since <= at < until` window.
/// Times are returned as RFC 3339 UTC with milliseconds.
fn filtered_query(query: &AuditQuery) -> QueryBuilder<'static, Postgres> {
    let mut builder = QueryBuilder::new(
        "select id::text as id, \
         to_char(at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') as at, \
         kind, payload \
         from audit_events where kind = any(",
    );
    builder.push_bind(
        query
            .kinds()
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect::<Vec<_>>(),
    );
    builder.push("::text[])");
    if let Some(symbol) = query.symbol() {
        builder.push(" and upper(payload->>'symbol') = ");
        builder.push_bind(symbol.to_owned());
    }
    if let Some(ticket) = query.ticket() {
        builder.push(" and (payload->>'ticket' = ");
        builder.push_bind(ticket.to_string());
        builder.push(" or payload->'result'->>'ticket' = ");
        builder.push_bind(ticket.to_string());
        builder.push(")");
    }
    if !query.command_ids().is_empty() {
        builder.push(" and payload->>'command_id' = any(");
        builder.push_bind(query.command_ids().to_vec());
        builder.push("::text[])");
    }
    if let Some(outcome) = query.outcome() {
        builder.push(" and payload->>'outcome' = ");
        builder.push_bind(outcome.to_owned());
    }
    // Unix milliseconds stay exact as float8 seconds far beyond any real date.
    if let Some(since) = query.since_ms() {
        builder.push(" and at >= to_timestamp(");
        builder.push_bind(since as f64 / 1_000.0);
        builder.push("::double precision)");
    }
    if let Some(until) = query.until_ms() {
        builder.push(" and at < to_timestamp(");
        builder.push_bind(until as f64 / 1_000.0);
        builder.push("::double precision)");
    }
    builder.push(" order by at desc, id desc limit ");
    builder.push_bind(i64::from(query.limit()));
    builder
}

fn storage_error(action: &str, error: &impl std::fmt::Display) -> AuditError {
    AuditError::Storage {
        reason: format!("{action} failed: {error}"),
    }
}

fn state_error(action: &str, error: &impl std::fmt::Display) -> StateError {
    StateError::Storage {
        reason: format!("{action} failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditKind;

    #[test]
    fn filtered_query_binds_every_value_and_adds_only_present_clauses() {
        let minimal = AuditQuery::new(&[AuditKind::ProposalEvaluated], 20).expect("query");
        assert_eq!(
            filtered_query(&minimal).sql(),
            "select id::text as id, \
             to_char(at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') as at, \
             kind, payload \
             from audit_events where kind = any($1::text[]) \
             order by at desc, id desc limit $2"
        );

        let full = AuditQuery::new(
            &[AuditKind::ProposalEvaluated, AuditKind::PositionClosed],
            50,
        )
        .and_then(|query| query.with_symbol("usdjpy"))
        .and_then(|query| query.with_ticket(10_654_130))
        .and_then(|query| {
            query.with_command_ids(&["5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10".to_owned()])
        })
        .and_then(|query| query.with_outcome("held"))
        .and_then(|query| query.with_window(Some(1_000), Some(2_000)))
        .expect("query");
        let builder = filtered_query(&full);
        let sql = builder.sql();
        for clause in [
            "where kind = any($1::text[])",
            " and upper(payload->>'symbol') = $2",
            " and (payload->>'ticket' = $3 or payload->'result'->>'ticket' = $4)",
            " and payload->>'command_id' = any($5::text[])",
            " and payload->>'outcome' = $6",
            " and at >= to_timestamp($7::double precision)",
            " and at < to_timestamp($8::double precision)",
        ] {
            assert!(sql.contains(clause), "missing `{clause}` in {sql}");
        }
        assert!(
            sql.ends_with(" order by at desc, id desc limit $9"),
            "{sql}"
        );
        assert!(
            !sql.contains("USDJPY") && !sql.contains("10654130") && !sql.contains("held"),
            "values are bound, never spliced: {sql}"
        );
    }
}
