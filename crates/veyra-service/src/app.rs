//! HTTP assembly shared by the process and integration tests.
//! Diagnostic routes, deterministic evaluation, the read-only market view, and
//! the loopback control surface; execution routes refuse unless both operator
//! controls are enabled.

use crate::AppState;
use actix_web::{App, Error, body::BoxBody, dev, web};

/// Creates the complete diagnostic application with non-cacheable responses.
pub fn create_app(
    state: AppState,
) -> App<
    impl dev::ServiceFactory<
        dev::ServiceRequest,
        Config = (),
        Response = dev::ServiceResponse<BoxBody>,
        Error = Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(web::Data::new(state))
        .wrap(actix_web::middleware::DefaultHeaders::new().add(("Cache-Control", "no-store")))
        .service(crate::routes::health)
        .service(crate::routes::readiness)
        .service(crate::routes::status)
        .service(crate::routes::metrics)
        .service(crate::routes::log_tail)
        .service(crate::routes::evaluate_intent)
        .service(crate::control::check_intent)
        .service(crate::control::execute_intent)
        .service(crate::control::close_position)
        .service(crate::control::modify_position)
        .service(crate::control::reconciliation)
        .service(crate::control::market_candles)
        .service(crate::control::audit_log)
        .service(crate::control::request_account_snapshot)
        .service(crate::control::command_status)
        .service(crate::control::command_list)
        .service(crate::control::event_feed)
        .service(crate::control::account_state)
}
