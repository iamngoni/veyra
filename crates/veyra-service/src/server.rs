//! Network lifecycle: bind a configured address, build the Actix server, and
//! await it to completion. Production stop signals are Actix's built-in
//! SIGINT/SIGTERM handling; tests stop the server through its handle.
//!
//! This module owns socket and worker lifecycle only. Route behavior lives in
//! `app`, and no execution capability is reachable from this surface.

use std::io;
use std::net::TcpListener;

use actix_web::dev::Server;

use crate::AppState;
use crate::config::ServiceConfig;

/// Binds a non-blocking listener for the configured address.
///
/// Startup fails instead of falling back to another port, so an operator never
/// believes the service is listening where it is not.
pub fn bind(config: &ServiceConfig) -> io::Result<TcpListener> {
    let listener = TcpListener::bind(config.address())?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Builds the diagnostic server on an already-prepared listener.
///
/// The server is not awaited here; callers either await it through [`serve`]
/// or hold its handle so a test can stop it deterministically.
pub fn build_server(state: AppState, listener: TcpListener) -> io::Result<Server> {
    let address = listener.local_addr()?;
    let server = actix_web::HttpServer::new(move || crate::app::create_app(state.clone()))
        .workers(2)
        .shutdown_timeout(10)
        .listen(listener)?
        .run();
    tracing::info!(%address, "Veyra diagnostic server ready; execution unavailable");
    Ok(server)
}

/// Awaits server completion after a graceful or forced stop.
///
/// Worker failure propagates as an error instead of being reported as a clean
/// shutdown.
pub async fn serve(server: Server) -> io::Result<()> {
    server.await?;
    tracing::info!("Veyra diagnostic server stopped");
    Ok(())
}
