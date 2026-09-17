//! Exercises accepted and rejected configuration boundaries without mutating
//! global environment state; process-environment behavior is tested separately.

use veyra_service::config::{ConfigError, Environment, Port, ServiceConfig};

fn config(host: &str, port: &str, env: &str) -> Result<ServiceConfig, ConfigError> {
    ServiceConfig::from_source(|name| {
        Ok(match name {
            "VEYRA_BIND_HOST" => host,
            "VEYRA_BIND_PORT" => port,
            "VEYRA_ENV" => env,
            _ => panic!("unexpected setting"),
        }
        .to_owned())
    })
}

#[test]
fn accepts_both_address_families_and_every_environment() {
    for (label, expected) in [
        ("production", Environment::Production),
        ("staging", Environment::Staging),
        ("development", Environment::Development),
    ] {
        for host in ["127.0.0.1", "::1", "0.0.0.0"] {
            let config = config(host, "9090", label).unwrap();
            assert_eq!(config.address().ip().to_string(), host);
            assert_eq!(config.address().port(), 9090);
            assert_eq!(config.environment(), expected);
            assert_eq!(config.environment().to_string(), label);
            assert!(!config.trading_enabled());
        }
    }
}

#[test]
fn rejects_invalid_addresses_without_echoing_input() {
    for host in [
        "localhost",
        "http://localhost",
        "127.0.0.1:80",
        "256.0.0.1",
        "127.0.0.1\n",
    ] {
        let error = config(host, "8080", "development").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid environment variable `VEYRA_BIND_HOST`: must be an IPv4 or IPv6 literal"
        );
    }
}

#[test]
fn enforces_port_boundaries() {
    for port in ["1024", "65535"] {
        assert_eq!(Port::parse(port).unwrap().value().to_string(), port);
    }
    for port in ["0", "80", "1023", "65536", "-1", "", "abc", " 8080"] {
        assert!(matches!(
            Port::parse(port),
            Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_BIND_PORT",
                ..
            })
        ));
    }
    assert!(config("127.0.0.1", "no", "development").is_err());
}

#[test]
fn rejects_missing_blank_and_unknown_settings() {
    for missing in ["VEYRA_BIND_HOST", "VEYRA_BIND_PORT", "VEYRA_ENV"] {
        let error = ServiceConfig::from_source(|name| {
            if name == missing {
                return Err(ConfigError::MissingEnvironmentVariable { name });
            }
            Ok(match name {
                "VEYRA_BIND_HOST" => "127.0.0.1",
                "VEYRA_BIND_PORT" => "8080",
                _ => "development",
            }
            .to_owned())
        })
        .unwrap_err();
        assert!(error.to_string().contains(missing));
    }
    for blank in ["", " \t\n"] {
        assert!(config(blank, "8080", "development").is_err());
        assert!(config("127.0.0.1", blank, "development").is_err());
        assert!(config("127.0.0.1", "8080", blank).is_err());
    }
    for env in ["prod", "DEVELOPMENT", "development "] {
        assert!(matches!(
            config("127.0.0.1", "8080", env),
            Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_ENV",
                ..
            })
        ));
    }
}

#[test]
fn from_env_reads_process_settings() {
    // SAFETY: this is the only test in this binary that mutates process
    // environment variables, so no other test thread observes the change.
    unsafe {
        std::env::set_var("VEYRA_BIND_HOST", "127.0.0.1");
        std::env::set_var("VEYRA_BIND_PORT", "8080");
        std::env::set_var("VEYRA_ENV", "staging");
    }

    let config = ServiceConfig::from_env().expect("process settings must parse");
    assert_eq!(config.address().port(), 8080);
    assert_eq!(config.environment(), Environment::Staging);

    unsafe {
        std::env::remove_var("VEYRA_BIND_HOST");
        std::env::remove_var("VEYRA_BIND_PORT");
        std::env::remove_var("VEYRA_ENV");
    }
}
