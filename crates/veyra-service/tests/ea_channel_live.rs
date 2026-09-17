//! Live round-trip proof for the EA control channel. Ignored by default.
//!
//! Requirements: MT4 running with the compiled `VeyraProbe` EA attached, the
//! endpoint allowlisted in the terminal, and the process environment sourced
//! from `.env` (VEYRA_BROKER_PROVIDER=ea, VEYRA_EA_TOKEN set).
//!
//! Run with: `cargo test --test ea_channel_live -- --ignored --nocapture`

use std::sync::Arc;
use std::time::{Duration, Instant};

use veyra_service::broker::{BrokerLink, BrokerSettings, EaLink, build_server};

#[actix_web::test]
#[ignore = "requires the VeyraProbe EA attached to a running MT4 terminal"]
async fn ea_probe_round_trip() {
    let settings = BrokerSettings::from_env()
        .expect("broker settings must parse")
        .expect("VEYRA_BROKER_PROVIDER=ea must be configured");
    let BrokerSettings::Ea(ea_settings) = settings;

    let link = Arc::new(EaLink::new(
        ea_settings.token().clone(),
        Duration::from_secs(30),
    ));
    let server = build_server(link.clone(), ea_settings.bind()).expect("EA endpoint must bind");
    let handle = server.handle();
    let task = actix_web::rt::spawn(server);

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut saw_heartbeat = false;
    while Instant::now() < deadline {
        if link.report().await.snapshot.is_some() {
            saw_heartbeat = true;
        }
        if saw_heartbeat && link.pongs_received() > 0 {
            break;
        }
        actix_web::rt::time::sleep(Duration::from_millis(250)).await;
    }

    handle.stop(true).await;
    let _ = task.await;

    assert!(
        saw_heartbeat,
        "no EA heartbeat received on {}",
        ea_settings.bind()
    );
    assert!(
        link.pongs_received() > 0,
        "EA did not answer ping with pong"
    );
}
