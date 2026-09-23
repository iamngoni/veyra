//! Process-wide JSON tracing setup. A bad log filter fails startup rather than
//! silently suppressing diagnostics; no request bodies or credentials are logged.
//! A capture layer also tees events into the bounded console log buffer.

use std::sync::Arc;

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::logs::{LogBuffer, LogLayer};

/// Builds the tracing filter. Only an absent `RUST_LOG` selects `info`;
/// blank, malformed, and unreadable values are startup errors.
fn build_filter(
    source: impl FnOnce() -> Result<String, std::env::VarError>,
) -> Result<EnvFilter, Box<dyn std::error::Error>> {
    let filter = match source() {
        Ok(value) => EnvFilter::try_new(value)?,
        Err(std::env::VarError::NotPresent) => EnvFilter::new("info"),
        Err(error) => return Err(error.into()),
    };
    // `agent-runtime` includes upstream response bodies in warning fields.
    // Veyra emits its own bounded model diagnostics, so never let those raw
    // dependency events reach stderr or the console log buffer.
    Ok(filter.add_directive("agent_runtime=error".parse()?))
}

/// Installs JSON logging exactly once and tees events into `logs` for the
/// console. Malformed `RUST_LOG` and duplicate setup are returned as errors so
/// operators cannot mistake partial telemetry for health.
pub fn init(logs: Arc<LogBuffer>) -> Result<(), Box<dyn std::error::Error>> {
    let filter = build_filter(|| std::env::var("RUST_LOG"))?;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr),
        )
        .with(LogLayer::new(logs))
        .with(filter)
        .try_init()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_filter_falls_back_to_info() {
        let filter = build_filter(|| Err(std::env::VarError::NotPresent)).unwrap();
        assert!(filter.max_level_hint().is_some());
    }

    #[test]
    fn malformed_and_unreadable_filters_fail_startup() {
        assert!(build_filter(|| Ok("not a filter[".to_owned())).is_err());
        let error = build_filter(|| Err(std::env::VarError::NotUnicode("x".into()))).unwrap_err();
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn init_installs_once_and_rejects_duplicate_setup() {
    let logs = LogBuffer::new(8);
    assert!(
        init(logs.clone()).is_ok(),
        "first initialization must succeed"
    );
    assert!(init(logs).is_err(), "duplicate initialization must fail");
}
