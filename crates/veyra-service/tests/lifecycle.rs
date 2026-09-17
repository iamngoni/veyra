//! Verifies the real socket lifecycle through the public API: bind failure on
//! an occupied address, HTTP serving, and graceful stop through the handle.
//! Process environment is never mutated here.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use veyra_service::broker::{EaLink, EaToken, build_server as build_ea_server};
use veyra_service::{AppState, config::ServiceConfig, server};

fn config(host: &str, port: &str) -> ServiceConfig {
    ServiceConfig::from_source(|name| {
        Ok(match name {
            "VEYRA_BIND_HOST" => host,
            "VEYRA_BIND_PORT" => port,
            "VEYRA_ENV" => "development",
            _ => unreachable!("unexpected setting"),
        }
        .to_owned())
    })
    .expect("test configuration must parse")
}

/// Reserves a currently free ephemeral port and binds it through the real
/// server path. Retries the tiny probe-drop race instead of being flaky.
fn bind_ephemeral() -> (TcpListener, SocketAddr) {
    for _ in 0..5 {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe listener");
        let probe_address = probe.local_addr().expect("probe address");
        let config = config("127.0.0.1", &probe_address.port().to_string());
        drop(probe);
        if let Ok(listener) = server::bind(&config) {
            let address = listener.local_addr().expect("bound address");
            return (listener, address);
        }
    }
    panic!("no free port available for the lifecycle test");
}

async fn get(address: SocketAddr, path: &str) -> Option<String> {
    let mut stream = tokio::net::TcpStream::connect(address).await.ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await.ok()?;
    Some(response)
}

#[test]
fn bind_fails_when_the_address_is_occupied() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("probe listener");
    let address = occupied.local_addr().expect("probe address");
    let config = config(&address.ip().to_string(), &address.port().to_string());

    let error = server::bind(&config).expect_err("occupied port must not fall back");
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
}

#[actix_web::test]
async fn serves_health_then_stops_gracefully() {
    let (listener, address) = bind_ephemeral();
    let state = AppState::new(config("127.0.0.1", &address.port().to_string()), None, None);
    let app = server::build_server(state, listener).expect("server build");
    let handle = app.handle();
    let task = actix_web::rt::spawn(server::serve(app, None));

    let mut response = None;
    for _ in 0..100 {
        if let Ok(Some(body)) =
            tokio::time::timeout(Duration::from_secs(1), get(address, "/health")).await
        {
            response = Some(body);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let response = response.expect("server must become ready");
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected response: {response}"
    );
    assert!(
        response.contains("\"status\":\"ok\""),
        "unexpected body: {response}"
    );

    handle.stop(true).await;
    task.await
        .expect("server task must not panic")
        .expect("graceful shutdown must succeed");
}

/// Address that was bound and immediately released; used to pre-select the
/// companion listener's port. The tiny reuse race is retried by the caller's
/// readiness loop instead of being flaky.
fn free_address() -> SocketAddr {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe listener");
    probe.local_addr().expect("probe address")
}

#[actix_web::test]
async fn companion_listener_starts_and_stops_with_main() {
    let (main_listener, main_addr) = bind_ephemeral();
    let main = server::build_server(
        AppState::new(
            config("127.0.0.1", &main_addr.port().to_string()),
            None,
            None,
        ),
        main_listener,
    )
    .expect("main server build");

    let link = Arc::new(EaLink::new(
        EaToken::parse("test-token-1234567890").expect("token"),
        Duration::from_secs(10),
        Duration::from_secs(5),
    ));
    let ea_addr = free_address();
    let ea = build_ea_server(link, ea_addr).expect("EA server build");

    let main_handle = main.handle();
    let task = actix_web::rt::spawn(server::serve(main, Some(ea)));

    let mut ready = false;
    for _ in 0..100 {
        let http_ok = get(main_addr, "/health").await.is_some();
        let ea_ok = tokio::net::TcpStream::connect(ea_addr).await.is_ok();
        if http_ok && ea_ok {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "both listeners must accept connections");

    main_handle.stop(true).await;
    task.await
        .expect("serve task must not panic")
        .expect("clean shutdown");

    let mut refused = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(ea_addr).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "companion listener must stop with the main server");
}
