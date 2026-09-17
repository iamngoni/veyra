//! Live PostgreSQL audit-trail proof. Ignored by default.
//!
//! Requires a reachable `VEYRA_DATABASE_URL` (source `.env`) and a server whose
//! role may create the `audit_events` table. Run with:
//! `cargo test --test store_live -- --ignored --nocapture`

use veyra_service::audit::{AuditEvent, AuditKind, AuditTrail};
use veyra_service::store::Store;

#[actix_web::test]
#[ignore = "requires VEYRA_DATABASE_URL and a running PostgreSQL server"]
async fn postgres_round_trips_audit_events() {
    let url =
        std::env::var("VEYRA_DATABASE_URL").expect("VEYRA_DATABASE_URL must be set (source .env)");
    let store = Store::connect(&url)
        .await
        .expect("postgres must accept the connection");
    store.migrate().await.expect("migrations must run");
    assert_eq!(store.provider().as_str(), "postgres");

    let marker = format!("live-proof-{}", uuid::Uuid::new_v4());
    store
        .record(AuditEvent::new(
            AuditKind::ServiceStarted,
            serde_json::json!({ "marker": marker }),
        ))
        .await
        .expect("append must succeed");

    let rows = store.recent(20).await.expect("read must succeed");
    assert!(
        rows.iter()
            .any(|row| row.payload["marker"] == marker.as_str()),
        "recorded marker must be readable"
    );

    let readable: Vec<_> = rows
        .iter()
        .take(5)
        .map(|row| format!("{} {} {}", row.at, row.kind, row.id))
        .collect();
    println!("recent audit rows:\n{}", readable.join("\n"));

    // Retention: an old row is pruned while recent rows survive.
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("direct pool for age-controlled inserts");
    let old_marker = format!("old-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "insert into audit_events (id, at, kind, payload) \
         values ($1::uuid, now() - interval '40 days', 'service_started', $2)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(serde_json::json!({ "marker": old_marker }))
    .execute(&pool)
    .await
    .expect("old row must insert");

    let pruned = store.delete_older_than(30).await.expect("prune must run");
    assert!(pruned >= 1, "the aged row must be pruned");

    let survivors = store.recent(50).await.expect("read after prune");
    assert!(
        !survivors
            .iter()
            .any(|row| row.payload["marker"] == old_marker.as_str()),
        "pruned markers must be gone"
    );
    assert!(
        survivors
            .iter()
            .any(|row| row.payload["marker"] == marker.as_str()),
        "recent rows must survive pruning"
    );
    assert_eq!(store.delete_older_than(0).await.expect("zero keeps"), 0);
    println!("pruned {pruned} aged row(s); recent rows survived");
}
