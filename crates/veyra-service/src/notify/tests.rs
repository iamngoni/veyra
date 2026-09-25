use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use actix_web::test as http;
use actix_web::{App, web};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::AppState;
use crate::credential::{TEST_ADMIN_TOKEN, test_vault};
use crate::state::test_support::MemoryState;
use crate::state::{RuntimeState, StateKey};

fn patch(value: Value) -> NotifyPatch {
    serde_json::from_value(value).expect("patch parses")
}

fn fields(rejected: &[Rejection]) -> Vec<&str> {
    rejected.iter().map(|item| item.field.as_str()).collect()
}

/// Accepts connections and answers each request with the next status in
/// `statuses` (the last one repeats), recording every request body.
async fn responder(statuses: Vec<u16>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = format!("http://{}", listener.local_addr().expect("address"));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    actix_web::rt::spawn(async move {
        let mut served = 0;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        seen.lock().expect("bodies").push(body.to_owned());
                        break;
                    }
                }
            }
            let status = statuses[served.min(statuses.len() - 1)];
            served += 1;
            let reply =
                format!("HTTP/1.1 {status} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno");
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (address, bodies)
}

fn ntfy_config(server: &str) -> NotifyConfig {
    NotifyConfig::default()
        .patched(&patch(json!({
            "providers": {
                "ntfy": {
                    "enabled": true,
                    "fields": { "server": server },
                    "secrets": { "topic": "veyra_alerts-01" }
                }
            }
        })))
        .expect("valid ntfy config")
}

#[test]
fn events_and_providers_round_trip_their_wire_names() {
    for event in NotifyEvent::ALL {
        assert_eq!(NotifyEvent::parse(event.as_str()), Some(event));
    }
    for kind in ProviderKind::ALL {
        assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(NotifyEvent::parse("nope"), None);
    assert_eq!(ProviderKind::parse("sms"), None);
}

#[test]
fn notifications_are_clipped_for_every_provider() {
    let long = "x".repeat(5_000);
    let notification = Notification::new(NotifyEvent::TradeClosed, Severity::Info, &long, &long);
    assert!(notification.title.chars().count() <= MAX_TITLE_CHARS + 1);
    assert!(notification.body.chars().count() <= MAX_BODY_CHARS + 1);
    assert_eq!(notification.event_name(), "trade_closed");
    assert_eq!(Notification::test().event_name(), "test");
}

#[test]
fn every_event_is_on_by_default_and_providers_are_off() {
    let config = NotifyConfig::default();
    for event in NotifyEvent::ALL {
        assert!(config.prefs.event_enabled(event));
    }
    assert!(config.enabled_providers().is_empty());
    assert!(
        !config.wants(NotifyEvent::TradeClosed),
        "no provider, nothing wanted"
    );
}

#[test]
fn patches_reject_unknown_names_and_misplaced_secrets() {
    let rejected = NotifyConfig::default()
        .patched(&patch(json!({
            "events": { "nope": true },
            "summaryHourUtc": 24,
            "providers": {
                "sms": {},
                "telegram": {
                    "fields": { "botToken": "in the wrong place" },
                    "secrets": { "chatId": "123", "color": "red" }
                }
            }
        })))
        .expect_err("rejected");
    let mut names = fields(&rejected);
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "events.nope",
            "providers.sms",
            "providers.telegram.botToken",
            "providers.telegram.chatId",
            "providers.telegram.color",
            "summaryHourUtc",
        ]
    );
}

#[test]
fn an_enabled_provider_needs_its_required_values() {
    let rejected = NotifyConfig::default()
        .patched(&patch(
            json!({ "providers": { "telegram": { "enabled": true } } }),
        ))
        .expect_err("incomplete");
    let mut names = fields(&rejected);
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["providers.telegram.botToken", "providers.telegram.chatId"]
    );
    // A disabled provider may be saved half-filled.
    let draft = NotifyConfig::default()
        .patched(&patch(
            json!({ "providers": { "telegram": { "fields": { "chatId": "-100123" } } } }),
        ))
        .expect("disabled drafts are allowed");
    assert!(draft.enabled_providers().is_empty());
}

#[test]
fn provider_values_are_validated() {
    let check = |kind: ProviderKind, values: &[(&str, &str)]| {
        let values = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        let rejected = kind.validate(&values);
        rejected
            .into_iter()
            .map(|item| item.field)
            .collect::<Vec<_>>()
    };
    assert!(
        check(
            ProviderKind::Telegram,
            &[("chatId", "-1001234"), ("botToken", "t")]
        )
        .is_empty()
    );
    assert!(
        check(
            ProviderKind::Telegram,
            &[("chatId", "@veyra_alerts"), ("botToken", "t")]
        )
        .is_empty()
    );
    assert_eq!(
        check(
            ProviderKind::Telegram,
            &[("chatId", "my chat"), ("botToken", "t")]
        ),
        vec!["providers.telegram.chatId"]
    );
    assert!(
        check(
            ProviderKind::Discord,
            &[("webhookUrl", "https://discord.com/api/webhooks/1/abc")]
        )
        .is_empty()
    );
    assert_eq!(
        check(
            ProviderKind::Discord,
            &[("webhookUrl", "https://evil.example/api/webhooks/1")]
        ),
        vec!["providers.discord.webhookUrl"]
    );
    assert!(
        check(
            ProviderKind::Slack,
            &[("webhookUrl", "https://hooks.slack.com/services/T/B/x")]
        )
        .is_empty()
    );
    assert_eq!(
        check(
            ProviderKind::Slack,
            &[("webhookUrl", "http://hooks.slack.com/services/T")]
        ),
        vec!["providers.slack.webhookUrl"]
    );
    assert_eq!(
        check(ProviderKind::Webhook, &[("url", "http://example.com/hook")]),
        vec!["providers.webhook.url"]
    );
    assert_eq!(
        check(
            ProviderKind::Ntfy,
            &[("topic", "has spaces"), ("server", "ntfy.sh")]
        ),
        vec!["providers.ntfy.server", "providers.ntfy.topic"]
    );
    assert!(
        check(
            ProviderKind::Email,
            &[
                ("host", "smtp.gmail.com"),
                ("from", "Veyra <bot@example.com>"),
                ("to", "a@example.com, b@example.com"),
                ("username", "bot@example.com"),
                ("password", "app-password")
            ]
        )
        .is_empty()
    );
    let mut email = check(
        ProviderKind::Email,
        &[
            ("host", "smtp.example.com"),
            ("from", "nope"),
            ("to", "a@example.com,,"),
            ("port", "0"),
            ("security", "plain"),
            ("username", "u"),
        ],
    );
    email.sort_unstable();
    assert_eq!(
        email,
        vec![
            "providers.email.from",
            "providers.email.password",
            "providers.email.port",
            "providers.email.security",
        ]
    );
    assert_eq!(
        check(
            ProviderKind::Email,
            &[("host", "h"), ("from", "a@example.com"), ("to", " , ")]
        ),
        vec!["providers.email.to"]
    );
}

#[test]
fn null_clears_a_value_and_empty_secret_maps_are_pruned() {
    let config = NotifyConfig::default()
        .patched(&patch(json!({ "providers": { "webhook": { "secrets": { "url": "https://example.com/h" } } } })))
        .expect("saved");
    assert!(config.secrets.contains_key(&ProviderKind::Webhook));
    let cleared = config
        .patched(&patch(
            json!({ "providers": { "webhook": { "secrets": { "url": null } } } }),
        ))
        .expect("cleared");
    assert!(!cleared.secrets.contains_key(&ProviderKind::Webhook));
    let blank = config
        .patched(&patch(
            json!({ "providers": { "webhook": { "secrets": { "url": "  " } } } }),
        ))
        .expect("blank clears too");
    assert!(!blank.secrets.contains_key(&ProviderKind::Webhook));
}

#[test]
fn the_view_redacts_every_secret() {
    let (notifier, _worker) = Notifier::new(true).expect("notifier");
    let token = "123456:ABCdefGHIjklMNOpqrSTUvwxYZ";
    notifier.set_config(
        NotifyConfig::default()
            .patched(&patch(json!({
                "events": { "trade_opened": false },
                "providers": { "telegram": { "enabled": true, "fields": { "chatId": "42" }, "secrets": { "botToken": token } } }
            })))
            .expect("valid"),
    );
    let view = notifier.view();
    assert!(!view.to_string().contains(token), "secret leaked: {view}");
    assert_eq!(view["available"], true);
    assert_eq!(view["events"]["trade_opened"], false);
    assert_eq!(view["events"]["trade_closed"], true);
    assert_eq!(view["providers"]["telegram"]["enabled"], true);
    assert_eq!(view["providers"]["telegram"]["fields"]["chatId"], "42");
    assert_eq!(
        view["providers"]["telegram"]["secrets"]["botToken"]["set"],
        true
    );
    assert_eq!(
        view["providers"]["telegram"]["secrets"]["botToken"]["hint"],
        "wxYZ"
    );
    assert_eq!(
        view["providers"]["email"]["secrets"]["password"]["set"],
        false
    );
    assert_eq!(hint("short"), "", "short secrets get no hint");
}

#[actix_web::test]
async fn notify_queues_only_wanted_events_and_counts_drops() {
    let (notifier, worker) = Notifier::new(true).expect("notifier");
    let closed = Notification::new(NotifyEvent::TradeClosed, Severity::Info, "t", "b");
    assert!(!notifier.notify(closed.clone()), "no provider enabled");

    let mut config = ntfy_config("http://127.0.0.1:9");
    config.prefs.events.insert(NotifyEvent::TradeClosed, false);
    notifier.set_config(config);
    assert!(!notifier.notify(closed.clone()), "event switched off");
    assert!(!notifier.notify(Notification::test()), "tests never queue");
    assert!(notifier.notify(Notification::new(
        NotifyEvent::OrderFailed,
        Severity::Warning,
        "t",
        "b"
    )));

    // Without a running worker the queue fills and the overflow is dropped.
    for _ in 0..QUEUE_CAPACITY {
        notifier.notify(Notification::new(
            NotifyEvent::OrderFailed,
            Severity::Warning,
            "t",
            "b",
        ));
    }
    assert!(notifier.0.counters.dropped.load(Ordering::Relaxed) >= 1);
    drop(worker);

    let disabled = Notifier::disabled();
    disabled.set_config(ntfy_config("http://127.0.0.1:9"));
    assert!(
        !disabled.notify(closed),
        "an unavailable notifier never queues"
    );
}

#[actix_web::test]
async fn the_worker_fans_out_and_retries_transient_failures() {
    let (server, bodies) = responder(vec![503, 200]).await;
    let (notifier, worker) = Notifier::new(true).expect("notifier");
    notifier.set_config(ntfy_config(&server));
    actix_web::rt::spawn(worker.run());

    assert!(notifier.notify(Notification::new(
        NotifyEvent::BreakerTripped,
        Severity::Critical,
        "Daily loss limit reached",
        "New entries are blocked until tomorrow."
    )));
    for _ in 0..200 {
        if notifier.0.counters.delivered.load(Ordering::Relaxed) == 1 {
            break;
        }
        actix_web::rt::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(notifier.0.counters.delivered.load(Ordering::Relaxed), 1);
    let view = notifier.view();
    assert_eq!(view["recent"][0]["ok"], true);
    assert_eq!(view["recent"][0]["attempts"], 2);
    assert_eq!(view["recent"][0]["provider"], "ntfy");
    let bodies = bodies.lock().expect("bodies");
    let sent: Value = serde_json::from_str(bodies.last().expect("delivered")).expect("json");
    assert_eq!(sent["topic"], "veyra_alerts-01");
    assert_eq!(sent["title"], "Daily loss limit reached");
    assert_eq!(sent["priority"], 5);
}

#[actix_web::test]
async fn a_test_message_reports_a_permanent_failure_without_retrying() {
    let (server, bodies) = responder(vec![401]).await;
    let (notifier, _worker) = Notifier::new(true).expect("notifier");
    notifier.set_config(ntfy_config(&server));
    let error = notifier
        .test(ProviderKind::Ntfy)
        .await
        .expect_err("rejected");
    assert!(error.starts_with("HTTP 401"), "{error}");
    assert_eq!(bodies.lock().expect("bodies").len(), 1, "no retry");
    assert_eq!(notifier.view()["recent"][0]["ok"], false);

    let incomplete = notifier
        .test(ProviderKind::Telegram)
        .await
        .expect_err("incomplete");
    assert!(incomplete.contains("required"), "{incomplete}");
}

fn app_state(vault: bool) -> (AppState, Arc<MemoryState>) {
    let config = crate::config::ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("config");
    let store = Arc::new(MemoryState::default());
    let (notifier, _worker) = Notifier::new(vault).expect("notifier");
    let state = AppState::new(
        config,
        None,
        None,
        crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
    )
    .with_runtime_state(RuntimeState::new(Some(store.clone())))
    .with_credential_vault(vault.then(test_vault))
    .with_notifier(notifier);
    (state, store)
}

#[actix_web::test]
async fn routes_require_the_operator_token_then_save_sealed_and_apply() {
    let (state, store) = app_state(true);
    let app = http::init_service(
        App::new()
            .app_data(web::Data::new(state.clone()))
            .service(routes::notifications)
            .service(routes::update_notifications)
            .service(routes::test_notification),
    )
    .await;
    let token = "123456:ABCdefGHIjklMNOpqrSTUvwxYZ";
    let body = json!({
        "providers": { "telegram": { "enabled": true, "fields": { "chatId": "42" }, "secrets": { "botToken": token } } }
    });

    let refused = http::call_service(
        &app,
        http::TestRequest::put()
            .uri("/notifications")
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert_eq!(refused.status(), 401);

    let invalid = http::call_service(
        &app,
        http::TestRequest::put()
            .uri("/notifications")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(json!({ "providers": { "telegram": { "enabled": true } } }))
            .to_request(),
    )
    .await;
    assert_eq!(invalid.status(), 400);
    let invalid: Value = http::read_body_json(invalid).await;
    assert_eq!(invalid["error"], "invalid_notifications");

    let saved = http::call_service(
        &app,
        http::TestRequest::put()
            .uri("/notifications")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert_eq!(saved.status(), 200);
    let saved: Value = http::read_body_json(saved).await;
    assert_eq!(saved["providers"]["telegram"]["enabled"], true);
    assert!(!saved.to_string().contains(token));

    let sealed = store.saved(StateKey::NotifySecrets).expect("secrets saved");
    assert!(!sealed.to_string().contains(token), "stored sealed");
    assert_eq!(
        store.saved(StateKey::NotifyPrefs).expect("prefs saved")["providers"]["telegram"]["fields"]
            ["chatId"],
        "42"
    );

    // A restart reads back exactly what was applied.
    let reloaded = routes::load(state.runtime_state(), &test_vault())
        .await
        .expect("reload");
    assert_eq!(reloaded, state.notifier().config());

    let listed = http::call_service(
        &app,
        http::TestRequest::get().uri("/notifications").to_request(),
    )
    .await;
    let listed: Value = http::read_body_json(listed).await;
    assert_eq!(
        listed["providers"]["telegram"]["secrets"]["botToken"]["set"],
        true
    );

    let unknown = http::call_service(
        &app,
        http::TestRequest::post()
            .uri("/notifications/test")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(json!({ "provider": "sms" }))
            .to_request(),
    )
    .await;
    assert_eq!(unknown.status(), 400);
}

#[actix_web::test]
async fn routes_refuse_changes_without_a_vault() {
    let (state, _) = app_state(false);
    let app = http::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .service(routes::notifications)
            .service(routes::update_notifications),
    )
    .await;
    let response = http::call_service(
        &app,
        http::TestRequest::put()
            .uri("/notifications")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(json!({}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 503);
    let listed = http::call_service(
        &app,
        http::TestRequest::get().uri("/notifications").to_request(),
    )
    .await;
    let listed: Value = http::read_body_json(listed).await;
    assert_eq!(listed["available"], false);
}

fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

async fn sent_body(
    kind: ProviderKind,
    pairs: Vec<(&str, String)>,
    status: u16,
) -> (Result<(), providers::SendError>, Value) {
    let (server, bodies) = responder(vec![status]).await;
    let pairs: Vec<(&str, String)> = pairs
        .into_iter()
        .map(|(name, value)| (name, value.replace("{server}", &server)))
        .collect();
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let notification = Notification::new(
        NotifyEvent::TradeClosed,
        Severity::Warning,
        "Closed GBPUSD short · −5.27",
        "Stop loss · 0.01 lots",
    );
    let client = build_client().expect("client");
    let result = providers::send(kind, &client, &values(&borrowed), &notification).await;
    let body = bodies
        .lock()
        .expect("bodies")
        .last()
        .map(|body| serde_json::from_str(body).expect("json body"))
        .unwrap_or(Value::Null);
    (result, body)
}

#[actix_web::test]
async fn chat_providers_send_their_own_message_shapes() {
    let (result, discord) = sent_body(
        ProviderKind::Discord,
        vec![("webhookUrl", "{server}/api/webhooks/1/x".to_owned())],
        204,
    )
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        discord["content"],
        "⚠️ **Closed GBPUSD short · −5.27**\nStop loss · 0.01 lots"
    );
    assert_eq!(discord["allowed_mentions"]["parse"], json!([]));

    let (result, slack) = sent_body(
        ProviderKind::Slack,
        vec![("webhookUrl", "{server}/services/T/B/x".to_owned())],
        200,
    )
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        slack["text"],
        "⚠️ *Closed GBPUSD short · −5.27*\nStop loss · 0.01 lots"
    );

    let (result, webhook) = sent_body(
        ProviderKind::Webhook,
        vec![
            ("url", "{server}/hook".to_owned()),
            ("bearerToken", "secret-token".to_owned()),
        ],
        200,
    )
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(webhook["service"], "veyra");
    assert_eq!(webhook["event"], "trade_closed");
    assert_eq!(webhook["severity"], "warning");
    assert_eq!(webhook["body"], "Stop loss · 0.01 lots");

    let (result, ntfy) = sent_body(
        ProviderKind::Ntfy,
        vec![
            ("server", "{server}/".to_owned()),
            ("topic", "veyra".to_owned()),
            ("accessToken", "tk_x".to_owned()),
        ],
        200,
    )
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(ntfy["priority"], 4);
    assert_eq!(ntfy["tags"], json!(["trade_closed"]));
}

#[actix_web::test]
async fn provider_failures_are_classified_and_carry_no_secrets() {
    let (result, _) = sent_body(
        ProviderKind::Webhook,
        vec![("url", "{server}/hook/super-secret-path".to_owned())],
        429,
    )
    .await;
    let error = result.expect_err("rate limited");
    assert!(error.transient());
    assert!(error.to_string().starts_with("HTTP 429"));

    let (result, _) = sent_body(
        ProviderKind::Slack,
        vec![("webhookUrl", "{server}/services/x".to_owned())],
        404,
    )
    .await;
    assert!(!result.expect_err("not found").transient());

    let client = build_client().expect("client");
    let notification = Notification::test();
    let missing = providers::send(ProviderKind::Telegram, &client, &values(&[]), &notification)
        .await
        .expect_err("missing token");
    assert_eq!(missing.to_string(), "botToken is not set");
    assert!(!missing.transient());
    for kind in [
        ProviderKind::Discord,
        ProviderKind::Pushover,
        ProviderKind::Webhook,
        ProviderKind::Ntfy,
    ] {
        assert!(
            providers::send(kind, &client, &values(&[]), &notification)
                .await
                .is_err()
        );
    }

    // A refused connection is transient and the reason never quotes the URL.
    let unreachable = providers::send(
        ProviderKind::Webhook,
        &client,
        &values(&[("url", "http://127.0.0.1:9/hook/super-secret-path")]),
        &notification,
    )
    .await
    .expect_err("refused");
    assert!(unreachable.transient());
    assert!(
        !unreachable.to_string().contains("super-secret-path"),
        "{unreachable}"
    );
}

#[actix_web::test]
async fn email_builds_the_message_before_connecting() {
    async fn send(pairs: &[(&str, &str)]) -> Result<(), providers::SendError> {
        providers::send(
            ProviderKind::Email,
            &reqwest::Client::new(),
            &values(pairs),
            &Notification::test(),
        )
        .await
    }
    assert_eq!(
        send(&[]).await.expect_err("no host").to_string(),
        "host is not set"
    );
    assert_eq!(
        send(&[("host", "127.0.0.1"), ("from", "nope")])
            .await
            .expect_err("bad from")
            .to_string(),
        "from is not an email address"
    );
    assert_eq!(
        send(&[("host", "127.0.0.1"), ("from", "a@example.com"), ("to", "")])
            .await
            .expect_err("no to")
            .to_string(),
        "to is not a list of email addresses"
    );
    assert_eq!(
        send(&[
            ("host", "127.0.0.1"),
            ("from", "a@example.com"),
            ("to", "b@example.com"),
            ("port", "x")
        ])
        .await
        .expect_err("bad port")
        .to_string(),
        "port is not a number"
    );
    assert_eq!(
        send(&[
            ("host", "127.0.0.1"),
            ("from", "a@example.com"),
            ("to", "b@example.com"),
            ("security", "none")
        ])
        .await
        .expect_err("bad security")
        .to_string(),
        "security is invalid"
    );
    // Nothing listens on port 9: the connection failure surfaces as an error.
    for security in ["starttls", "tls"] {
        let refused = send(&[
            ("host", "127.0.0.1"),
            ("port", "9"),
            ("security", security),
            ("from", "a@example.com"),
            ("to", "b@example.com"),
            ("username", "a@example.com"),
            ("password", "app-password"),
        ])
        .await
        .expect_err("refused");
        assert!(!refused.to_string().contains("app-password"));
    }
}

#[actix_web::test]
async fn the_test_route_reports_delivery_and_save_failures() {
    let (state, _) = app_state(true);
    let (server, _) = responder(vec![200]).await;
    state.notifier().set_config(ntfy_config(&server));
    let app = http::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .service(routes::update_notifications)
            .service(routes::test_notification),
    )
    .await;
    let sent = http::call_service(
        &app,
        http::TestRequest::post()
            .uri("/notifications/test")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(json!({ "provider": "ntfy" }))
            .to_request(),
    )
    .await;
    assert_eq!(sent.status(), 200);
    let failed = http::call_service(
        &app,
        http::TestRequest::post()
            .uri("/notifications/test")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(json!({ "provider": "telegram" }))
            .to_request(),
    )
    .await;
    assert_eq!(failed.status(), 502);
    let failed: Value = http::read_body_json(failed).await;
    assert_eq!(failed["error"], "notification_failed");

    let malformed = http::call_service(
        &app,
        http::TestRequest::put()
            .uri("/notifications")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(json!({ "unexpected": true }))
            .to_request(),
    )
    .await;
    assert_eq!(malformed.status(), 400);

    // A vault without a database cannot save.
    let (notifier, _worker) = Notifier::new(true).expect("notifier");
    let unsaved = AppState::new(
        app_state(false).0.config().clone(),
        None,
        None,
        crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
    )
    .with_credential_vault(Some(test_vault()))
    .with_notifier(notifier);
    let app = http::init_service(
        App::new()
            .app_data(web::Data::new(unsaved))
            .service(routes::update_notifications),
    )
    .await;
    let response = http::call_service(
        &app,
        http::TestRequest::put()
            .uri("/notifications")
            .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
            .set_json(json!({ "events": { "trade_opened": false } }))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 500);
    let body: Value = http::read_body_json(response).await;
    assert_eq!(body["error"], "notifications_not_saved");
}

#[actix_web::test]
async fn saved_settings_that_cannot_be_read_fail_loudly() {
    let store = Arc::new(MemoryState::default());
    let state = RuntimeState::new(Some(store.clone()));
    assert_eq!(
        routes::load(&state, &test_vault()).await.expect("empty"),
        NotifyConfig::default()
    );
    store
        .seed(StateKey::NotifyPrefs, json!({ "events": { "nope": true } }))
        .await;
    assert!(routes::load(&state, &test_vault()).await.is_err());
    store.seed(StateKey::NotifyPrefs, json!({})).await;
    store
        .seed(
            StateKey::NotifySecrets,
            test_vault().seal_text("not json").expect("sealed"),
        )
        .await;
    assert!(routes::load(&state, &test_vault()).await.is_err());
    store
        .seed(
            StateKey::NotifySecrets,
            json!({ "version": 1, "ciphertext": null }),
        )
        .await;
    assert!(
        routes::load(&state, &test_vault())
            .await
            .expect("tombstone")
            .secrets
            .is_empty()
    );
}

async fn delivered(notifier: &Notifier, count: u64) -> bool {
    for _ in 0..300 {
        if notifier.0.counters.delivered.load(Ordering::Relaxed) >= count {
            return true;
        }
        actix_web::rt::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[actix_web::test]
async fn watchers_turn_journal_entries_into_deliveries() {
    let (server, bodies) = responder(vec![200]).await;
    let trail = Arc::new(crate::audit::MemoryTrail::default());
    trail.record_at(
        OffsetDateTime::now_utc() - time::Duration::hours(2),
        crate::audit::AuditEvent::new(
            crate::audit::AuditKind::PositionClosed,
            json!({"ticket": 1, "symbol": "EURUSD", "profit": 1.5}),
        ),
    );
    let audit = crate::audit::AuditRuntime::new(trail);
    let (notifier, worker) = Notifier::new(true).expect("notifier");
    notifier.set_config(ntfy_config(&server));
    actix_web::rt::spawn(worker.run());
    let (state, _) = app_state(true);
    let state = state
        .with_audit(Some(audit.clone()))
        .with_notifier(notifier.clone());

    events::spawn(&state);
    // Let the audit watcher take its starting cursor before recording.
    actix_web::rt::time::sleep(Duration::from_millis(50)).await;
    audit
        .try_record(crate::audit::AuditEvent::new(
            crate::audit::AuditKind::PositionClosed,
            json!({"ticket": 2, "symbol": "GBPUSD", "kind": "sell", "profit": -5.27}),
        ))
        .await;
    assert!(delivered(&notifier, 1).await, "close delivered");
    let sent: Value =
        serde_json::from_str(bodies.lock().expect("bodies").last().expect("body")).expect("json");
    assert_eq!(sent["title"], "Closed GBPUSD short · −5.27");

    // No broker: nothing about the link is known, so nothing is reported.
    let sample = events::sample(&state).await;
    assert_eq!(sample.broker_fresh, None);
    assert_eq!(sample.model_failures.0, 0);

    events::send_summary(&state).await;
    assert!(delivered(&notifier, 2).await, "summary delivered");
    let summary: Value =
        serde_json::from_str(bodies.lock().expect("bodies").last().expect("body")).expect("json");
    assert_eq!(summary["title"], "Daily summary");
    assert_eq!(summary["message"], "2 closed · 1 won · 1 lost · net −3.77");
}

#[actix_web::test]
async fn watchers_do_not_start_without_a_vault() {
    let (state, _) = app_state(false);
    events::spawn(&state);
    assert!(!state.notifier().available());
}

#[actix_web::test]
async fn the_watchdog_probe_reads_readiness() {
    let client = build_client().expect("client");
    let (up, _) = responder(vec![200]).await;
    assert_eq!(
        watchdog::probe(&client, &format!("{up}/ready")).await,
        watchdog::Probe::Ready
    );
    let (down, _) = responder(vec![503]).await;
    assert_eq!(
        watchdog::probe(&client, &format!("{down}/ready")).await,
        watchdog::Probe::Unreachable("/ready answered HTTP 503".to_owned())
    );
    assert!(matches!(
        watchdog::probe(&client, "http://127.0.0.1:9/ready").await,
        watchdog::Probe::Unreachable(_)
    ));
}
