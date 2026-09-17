//! Minimal process wiring: fail before listening if configuration or logging is
//! invalid, then await the diagnostic server. This binary has no execution path.

use veyra_service::{AppState, config::ServiceConfig, observability, server};

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServiceConfig::from_env()?;
    observability::init()?;
    let listener = server::bind(&config)?;
    let app = server::build_server(AppState::new(config), listener)?;
    server::serve(app).await?;
    Ok(())
}
