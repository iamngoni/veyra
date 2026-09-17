//! Verifies the real socket lifecycle through the public API: bind failure on
//! an occupied address, HTTP serving, and graceful stop through the handle.
//! Process environment is never mutated here.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let state = AppState::new(config("127.0.0.1", &address.port().to_string()));
    let app = server::build_server(state, listener).expect("server build");
    let handle = app.handle();
    let task = actix_web::rt::spawn(server::serve(app));

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
