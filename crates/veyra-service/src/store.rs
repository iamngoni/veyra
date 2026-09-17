//! PostgreSQL audit storage (SQLx).
//!
//! Thin glue between the audit boundary and the `audit_events` table: connect
//! with bounded pooling and timeouts, run the embedded migrations, append
//! events, and read the newest rows. Behaviour depends on a running server, so
//! this module is exercised by `tests/store_live.rs` and the smoke script
//! rather than the deterministic suite (see `.cargo/config.toml`).

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::audit::{AuditError, AuditEvent, AuditProvider, AuditRow, AuditTrail};

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
            .acquire_timeout(Duration::from_secs(5))
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

    async fn prune(&self, keep_days: u32) -> Result<u64, AuditError> {
        self.delete_older_than(keep_days).await
    }
}

fn storage_error(action: &str, error: &impl std::fmt::Display) -> AuditError {
    AuditError::Storage {
        reason: format!("{action} failed: {error}"),
    }
}
