//! Live PostgreSQL audit-trail proof. Ignored by default.
//!
//! Requires a reachable `VEYRA_DATABASE_URL` (source `.env`) and a server whose
//! role may create the `audit_events` table. Run with:
//! `cargo test --test store_live -- --ignored --nocapture`

use veyra_service::audit::{AuditEvent, AuditKind, AuditTrail};
use veyra_service::state::{StateKey, StateStore};
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

    // Runtime state round-trips through the same database.
    let value = serde_json::json!({ "calls": 42, "marker": marker });
    store
        .save(StateKey::JevUsage.as_str(), &value)
        .await
        .expect("state save must succeed");
    let loaded = store
        .load(StateKey::JevUsage.as_str())
        .await
        .expect("state load must succeed");
    assert_eq!(loaded, Some(value), "stored state must read back");
    store
        .save(
            StateKey::JevUsage.as_str(),
            &serde_json::json!({ "calls": 43 }),
        )
        .await
        .expect("state replace must succeed");
    assert_eq!(
        store
            .load(StateKey::JevUsage.as_str())
            .await
            .expect("state reload"),
        Some(serde_json::json!({ "calls": 43 })),
        "saves replace the previous value"
    );
    println!("runtime state round-tripped");
}

#[actix_web::test]
#[ignore = "requires VEYRA_DATABASE_URL and a running PostgreSQL server"]
async fn postgres_filtered_queries_follow_the_shared_semantics() {
    use veyra_service::audit::{AuditQuery, parse_trail_time};

    let url =
        std::env::var("VEYRA_DATABASE_URL").expect("VEYRA_DATABASE_URL must be set (source .env)");
    let store = Store::connect(&url)
        .await
        .expect("postgres must accept the connection");
    store.migrate().await.expect("migrations must run");
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("direct pool for time-controlled inserts");

    // Unique identities keep this proof independent of existing rows.
    let ticket = 900_000_000_000_i64 + i64::from(uuid::Uuid::new_v4().as_fields().1);
    let open = uuid::Uuid::new_v4().to_string();
    let close = uuid::Uuid::new_v4().to_string();
    let symbol = format!("LIVE{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let fixtures = [
        (
            "120 minutes",
            "proposal_evaluated",
            serde_json::json!({"outcome": "queued", "symbol": symbol.to_lowercase(), "command_id": open}),
        ),
        (
            "119 minutes",
            "command_completed",
            serde_json::json!({"kind": "open_order", "command_id": open, "result": {"ticket": ticket}}),
        ),
        (
            "60 minutes",
            "proposal_evaluated",
            serde_json::json!({"outcome": "held", "symbol": symbol, "ticket": ticket.to_string()}),
        ),
        (
            "10 minutes",
            "proposal_evaluated",
            serde_json::json!({"outcome": "close_queued", "symbol": symbol, "ticket": ticket, "command_id": close}),
        ),
        (
            "5 minutes",
            "position_closed",
            serde_json::json!({"ticket": ticket, "symbol": symbol}),
        ),
    ];
    for (age, kind, payload) in &fixtures {
        sqlx::query(
            "insert into audit_events (id, at, kind, payload) \
             values ($1::uuid, now() - $2::interval, $3, $4)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(*age)
        .bind(*kind)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("fixture row must insert");
    }
    let run = |query: Result<AuditQuery, veyra_service::audit::AuditQueryError>| {
        let store = &store;
        async move {
            store
                .query(&query.expect("valid query"))
                .await
                .expect("filtered query runs")
        }
    };

    let by_ticket =
        run(AuditQuery::new(&AuditKind::ALL, 50).and_then(|q| q.with_ticket(ticket))).await;
    assert_eq!(
        by_ticket.len(),
        4,
        "ticket as number, as text, and as result.ticket"
    );
    let times: Vec<i64> = by_ticket
        .iter()
        .map(|row| parse_trail_time(&row.at).expect("RFC 3339 UTC time"))
        .collect();
    assert!(by_ticket.iter().all(|row| row.at.ends_with('Z')));
    assert!(
        times.windows(2).all(|pair| pair[0] >= pair[1]),
        "newest first"
    );

    let held = run(AuditQuery::new(&[AuditKind::ProposalEvaluated], 50)
        .and_then(|q| q.with_symbol(&symbol))
        .and_then(|q| q.with_outcome("held")))
    .await;
    assert_eq!(held.len(), 1, "upper-cased symbol plus exact outcome");

    let symbol_rows =
        run(AuditQuery::new(&[AuditKind::ProposalEvaluated], 50)
            .and_then(|q| q.with_symbol(&symbol)))
        .await;
    assert_eq!(symbol_rows.len(), 3, "symbol comparison ignores ASCII case");

    let linked = run(AuditQuery::new(&AuditKind::ALL, 50)
        .and_then(|q| q.with_command_ids(&[open.clone(), close.clone()])))
    .await;
    assert_eq!(
        linked.len(),
        3,
        "the entry decision is reachable by command id"
    );

    let window = run(AuditQuery::new(&AuditKind::ALL, 50)
        .and_then(|q| q.with_ticket(ticket))
        .and_then(|q| q.with_window(Some(times[3] + 1), Some(times[0]))))
    .await;
    assert_eq!(window.len(), 2, "since is inclusive and until exclusive");

    let limited =
        run(AuditQuery::new(&AuditKind::ALL, 1).and_then(|q| q.with_ticket(ticket))).await;
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].kind, "position_closed");
    println!("filtered queries verified for ticket {ticket}");
}
