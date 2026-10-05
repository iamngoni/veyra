//! Judge selection tests: route authentication and the OpenAI preconditions,
//! fallback through the shared runtime, sealed-key persistence, and restore
//! at startup. OpenAI is a local mock; nothing here reaches a live service.

use std::collections::BTreeMap;
use std::sync::Arc;

use actix_web::test as http;
use actix_web::{App, web};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::*;
use crate::audit::{AuditKind, AuditRuntime, MemoryTrail};
use crate::credential::{TEST_ADMIN_TOKEN, test_vault};
use crate::jev::openai::tests::{KEY, Mock, mock};
use crate::jev::{JevResponse, NoulCriteria};
use crate::state::test_support::{FailingState, MemoryState};
use crate::state::{StateError, StateStore};

const NOT_ENABLED: &str = r#"{"error":{"message":"Decision API is not enabled for this user.","type":"invalid_request_error","param":null,"code":null}}"#;

/// A contract-valid answer to [`probe_request`].
fn probe_answer() -> String {
    json!({
        "id": "dec_probe",
        "model": "gpt-6-luna-2026-09-01",
        "answers": {
            "direction": {"type": "choice", "choice": "long", "probabilities": {"long": 0.9, "short": 0.05, "flat": 0.05}, "confidence": 0.8},
            "trending": {"type": "noul", "noul": 0.82},
            "momentum": {"type": "score", "score": 1.8, "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"}, "probabilities": {"0": 0.05, "1": 0.1, "2": 0.85}, "confidence": 0.7}
        },
        "usage": {"input_tokens": 150, "output_tokens": 12}
    })
    .to_string()
}

/// The configured TypeSafe judge: always answers one noul question.
#[derive(Debug)]
struct Jev;

#[async_trait]
impl SemanticJudge for Jev {
    fn provider(&self) -> JevProvider {
        JevProvider::TypeSafe
    }

    async fn judge(&self, _request: JevRequest) -> Result<JevResponse, JevError> {
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {"trending": {"type": "noul", "noul": 0.4}},
            "usage": {"input_tokens": 10, "output_tokens": 2}
        });
        crate::jev::contract::parse_response_body(body.to_string().as_bytes())
    }
}

fn jev() -> JevRuntime {
    JevRuntime::with_judge(JevProvider::TypeSafe, Arc::new(Jev))
}

fn trending() -> JevRequest {
    let mut questions = BTreeMap::new();
    questions.insert(
        "trending".to_owned(),
        Question::noul(
            Instructions::text("Trending?").expect("instructions"),
            NoulCriteria::default(),
        ),
    );
    JevRequest::new(State::text("EURUSD").expect("state"), questions).expect("request")
}

fn control_with(base: &str, model: Option<&str>) -> JudgeControl {
    let settings = OpenAiSettings::from_source(|name| match (name, model) {
        ("VEYRA_JUDGE_OPENAI_BASE_URL", _) => Ok(base.to_owned()),
        ("VEYRA_JUDGE_OPENAI_MODEL", Some(model)) => Ok(model.to_owned()),
        _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("settings");
    JudgeControl::new(&settings).expect("control")
}

fn control(base: &str) -> JudgeControl {
    control_with(base, None)
}

fn base_state() -> AppState {
    let config = crate::config::ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("config");
    AppState::new(
        config,
        None,
        None,
        crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
    )
}

struct Harness {
    state: AppState,
    store: Arc<MemoryState>,
    server: Mock,
    trail: Arc<MemoryTrail>,
}

async fn harness(with_jev: bool) -> Harness {
    let server = mock(403, NOT_ENABLED.to_owned()).await;
    let store = Arc::new(MemoryState::default());
    let trail = Arc::new(MemoryTrail::default());
    let state = base_state()
        .with_runtime_state(RuntimeState::new(Some(store.clone())))
        .with_credential_vault(Some(test_vault()))
        .with_jev(with_jev.then(jev))
        .with_audit(Some(AuditRuntime::new(trail.clone())))
        .with_judge_control(Some(control(&server.base)));
    Harness {
        state,
        store,
        server,
        trail,
    }
}

macro_rules! app {
    ($state:expr) => {
        http::init_service(
            App::new()
                .app_data(web::Data::new($state.clone()))
                .service(routes::judge)
                .service(routes::select_judge)
                .service(routes::set_openai_key)
                .service(routes::delete_openai_key)
                .service(routes::test_openai)
                .service(crate::routes::status),
        )
        .await
    };
}

/// Sends a request and returns its status and JSON body (null when empty).
macro_rules! call {
    ($app:expr, $request:expr) => {{
        let response = http::call_service(&$app, $request.to_request()).await;
        let status = response.status().as_u16();
        let body = http::read_body(response).await;
        (
            status,
            serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
        )
    }};
}

fn get() -> http::TestRequest {
    http::TestRequest::get().uri("/judge")
}

fn authed(request: http::TestRequest) -> http::TestRequest {
    request.insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
}

fn put_key(key: &str) -> http::TestRequest {
    authed(http::TestRequest::put().uri("/judge/openai/key")).set_json(json!({ "key": key }))
}

fn delete_key() -> http::TestRequest {
    authed(http::TestRequest::delete().uri("/judge/openai/key"))
}

fn select(provider: &str) -> http::TestRequest {
    authed(http::TestRequest::put().uri("/judge")).set_json(json!({ "provider": provider }))
}

fn run_test() -> http::TestRequest {
    authed(http::TestRequest::post().uri("/judge/openai/test"))
}

#[test]
fn failed_probes_are_described_without_provider_jargon() {
    let cases = [
        (
            JevError::Denied {
                status: 403,
                detail: "Decision API is not enabled for this user.".to_owned(),
            },
            "Decision API is not enabled for this user. (403)",
        ),
        (JevError::Unauthorized, "Credential rejected"),
        (
            JevError::Rejected {
                detail: "bad model".to_owned(),
            },
            "Request rejected: bad model",
        ),
        (
            JevError::Unavailable { status: 429 },
            "Rate limited or overloaded (429)",
        ),
        (
            JevError::Transport {
                reason: "timeout".to_owned(),
            },
            "Unreachable: timeout",
        ),
        (
            JevError::MalformedResponse {
                reason: "not json".to_owned(),
            },
            "Unexpected answer format: not json",
        ),
        (
            JevError::Contract {
                reason: "missing answer".to_owned(),
            },
            "Unexpected answer format: missing answer",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(describe(&error), expected);
    }
    assert_eq!(unix_ms(UNIX_EPOCH), 0);
    assert_eq!(
        unix_ms(UNIX_EPOCH + std::time::Duration::from_millis(1_234)),
        1_234
    );
}

#[test]
fn the_probe_asks_one_question_of_each_type() {
    let probe = probe_request().expect("fixed probe is valid");
    let mut kinds: Vec<&str> = probe
        .questions()
        .values()
        .map(|question| match question {
            Question::Choice { .. } => "choice",
            Question::Noul { .. } => "noul",
            Question::Score { .. } => "score",
        })
        .collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ["choice", "noul", "score"]);
    let answer = crate::jev::contract::parse_response_body(probe_answer().as_bytes())
        .expect("answer parses");
    probe
        .validate_answers(&answer)
        .expect("answer fits the probe");
}

#[actix_web::test]
async fn openai_is_selectable_only_after_a_saved_key_and_a_passing_test() {
    let harness = harness(true).await;
    let app = app!(harness.state);

    let (status, view) = call!(app, get());
    assert_eq!(status, 200);
    assert_eq!(
        view,
        json!({
            "provider": "typesafe",
            "fallbackAvailable": true,
            "available": true,
            "openai": {
                "key": {"set": false, "hint": null},
                "model": "gpt-6-luna",
                "test": null,
                "fallbacks": 0
            }
        })
    );

    // Writes need the operator token.
    for request in [
        http::TestRequest::put()
            .uri("/judge/openai/key")
            .set_json(json!({ "key": KEY })),
        http::TestRequest::put()
            .uri("/judge")
            .set_json(json!({"provider": "openai"})),
        http::TestRequest::post().uri("/judge/openai/test"),
        http::TestRequest::delete().uri("/judge/openai/key"),
    ] {
        assert_eq!(call!(app, request).0, 401);
    }

    // Nothing to test or select yet.
    let (status, body) = call!(app, run_test());
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("openai_key_missing"))
    );
    let (status, body) = call!(app, select("openai"));
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("openai_key_missing"))
    );

    // Malformed keys are refused without being echoed.
    let (status, body) = call!(app, put_key("sk-has spaces in it 0123456789"));
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_openai_key"))
    );
    assert!(!body.to_string().contains("has spaces"));
    let (status, body) = call!(
        app,
        authed(http::TestRequest::put().uri("/judge/openai/key"))
            .set_json(json!({ "key": 7, "secret": "sk-leak-0123456789abcdef" }))
    );
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_openai_key"))
    );
    assert!(!body.to_string().contains("leak"));

    // Saved sealed; the view shows only a hint.
    let (status, view) = call!(app, put_key(KEY));
    assert_eq!(status, 200);
    assert_eq!(view["openai"]["key"], json!({"set": true, "hint": "WXYZ"}));
    assert!(!view.to_string().contains(KEY));
    let sealed = harness
        .store
        .saved(StateKey::JudgeOpenAiKey)
        .expect("key stored");
    assert!(!sealed.to_string().contains(KEY));
    assert_eq!(
        test_vault().open_text(&sealed).expect("opens").as_deref(),
        Some(KEY)
    );

    // Untested keys cannot be selected.
    let (status, body) = call!(app, select("openai"));
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("openai_test_required"))
    );

    // A "not enabled" preview answer is a failed test with its own message.
    let (status, body) = call!(app, run_test());
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false);
    assert_eq!(
        body["detail"],
        "Decision API is not enabled for this user. (403)"
    );
    assert!(body["latencyMs"].is_u64());
    let (status, body) = call!(app, select("openai"));
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("openai_test_required"))
    );

    // The probe went to the Decisions endpoint in the System One shape.
    {
        let requests = harness.server.requests.lock().expect("requests");
        let (head, body) = requests.last().expect("probe sent");
        assert!(head.starts_with("POST /v1/decisions "));
        let body: Value = serde_json::from_str(body).expect("json");
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["questions"]["direction"]["type"], "choice");
        assert_eq!(body["questions"]["trending"]["type"], "noul");
        assert_eq!(body["questions"]["momentum"]["type"], "score");
    }

    // A passing test unlocks the switch.
    harness.server.reply(200, probe_answer());
    let (status, body) = call!(app, run_test());
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["detail"], "Answered by gpt-6-luna-2026-09-01");
    let (_, view) = call!(app, get());
    assert_eq!(view["openai"]["test"]["ok"], true);
    assert_eq!(view["openai"]["test"]["model"], "gpt-6-luna");

    let (status, view) = call!(app, select("openai"));
    assert_eq!(status, 200);
    assert_eq!(view["provider"], "openai");
    let runtime = harness.state.jev().expect("jev");
    assert_eq!(runtime.provider(), JevProvider::OpenAi);
    let (_, status_body) = call!(app, http::TestRequest::get().uri("/status"));
    assert_eq!(status_body["jev_provider"], "openai");
    assert_eq!(
        harness.store.saved(StateKey::JudgePrefs).expect("prefs")["provider"],
        "openai"
    );

    // When OpenAI stops answering, the trade pipeline's handle gets Jev's
    // answer instead of an error.
    harness.server.reply(403, NOT_ENABLED.to_owned());
    let answer = runtime
        .evaluate(trending())
        .await
        .expect("fallback answers");
    assert_eq!(answer.model(), "jev-1.13.0");
    let (_, view) = call!(app, get());
    assert_eq!(view["openai"]["fallbacks"], 1);
    let (_, status_body) = call!(app, http::TestRequest::get().uri("/status"));
    assert_eq!(status_body["jev_usage"]["fallbacks"], 1);
    assert_eq!(status_body["jev_usage"]["failures"], 0);

    // A failing test withdraws OpenAI.
    let (_, body) = call!(app, run_test());
    assert_eq!(body["ok"], false);
    let (_, view) = call!(app, get());
    assert_eq!(view["provider"], "typesafe");
    assert_eq!(runtime.provider(), JevProvider::TypeSafe);

    // Changes are journaled without the key.
    let events = harness.trail.events();
    let changes: Vec<&str> = events
        .iter()
        .filter(|event| event.kind() == AuditKind::RuntimeConfigUpdated)
        .filter_map(|event| event.payload()["change"].as_str())
        .collect();
    assert_eq!(changes, ["openai_key_saved", "provider"]);
    assert!(
        !events
            .iter()
            .any(|event| event.payload().to_string().contains(KEY))
    );
}

#[actix_web::test]
async fn replacing_or_removing_the_key_returns_to_typesafe() {
    let harness = harness(true).await;
    harness.server.reply(200, probe_answer());
    let app = app!(harness.state);
    assert_eq!(call!(app, put_key(KEY)).0, 200);
    assert_eq!(call!(app, run_test()).1["ok"], true);
    assert_eq!(call!(app, select("openai")).0, 200);

    let (status, view) = call!(app, put_key("sk-proj-replacement_0123456789abcd"));
    assert_eq!(status, 200);
    assert_eq!(view["provider"], "typesafe");
    assert_eq!(view["openai"]["test"], Value::Null);
    assert_eq!(view["openai"]["key"]["hint"], "abcd");
    assert_eq!(
        harness.store.saved(StateKey::JudgePrefs).expect("prefs"),
        json!({"provider": "typesafe", "test": null})
    );

    assert_eq!(call!(app, run_test()).1["ok"], true);
    assert_eq!(call!(app, select("openai")).0, 200);
    let (status, view) = call!(app, delete_key());
    assert_eq!(status, 200);
    assert_eq!(view["provider"], "typesafe");
    assert_eq!(view["openai"]["key"], json!({"set": false, "hint": null}));
    assert_eq!(
        harness.store.saved(StateKey::JudgeOpenAiKey),
        Some(json!({"version": 1, "ciphertext": null}))
    );
    assert_eq!(
        harness.state.jev().expect("jev").provider(),
        JevProvider::TypeSafe
    );

    // TypeSafe is always selectable; bad bodies are refused by name.
    assert_eq!(call!(app, select("typesafe")).0, 200);
    let (status, body) = call!(app, select("anthropic"));
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("unknown_judge"))
    );
    let (status, body) = call!(
        app,
        authed(http::TestRequest::put().uri("/judge")).set_json(json!({"judge": "openai"}))
    );
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_judge"))
    );
}

#[actix_web::test]
async fn openai_needs_a_configured_jev_fallback() {
    let harness = harness(false).await;
    harness.server.reply(200, probe_answer());
    let app = app!(harness.state);
    assert_eq!(call!(app, put_key(KEY)).0, 200);
    assert_eq!(call!(app, run_test()).1["ok"], true);
    let (status, body) = call!(app, select("openai"));
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("openai_needs_fallback"))
    );
    let (_, view) = call!(app, get());
    assert_eq!(view["fallbackAvailable"], false);
    assert_eq!(view["provider"], "typesafe");
    assert_eq!(view["openai"]["fallbacks"], 0);
}

/// Store whose writes to the sealed key fail; everything else succeeds.
#[derive(Debug, Default)]
struct KeyWritesFail(MemoryState);

#[async_trait]
impl StateStore for KeyWritesFail {
    async fn load(&self, key: &str) -> Result<Option<Value>, StateError> {
        self.0.load(key).await
    }

    async fn save(&self, key: &str, value: &Value) -> Result<(), StateError> {
        if key == StateKey::JudgeOpenAiKey.as_str() {
            return Err(StateError::Storage {
                reason: "disk full".to_owned(),
            });
        }
        self.0.save(key, value).await
    }
}

#[actix_web::test]
async fn writes_need_a_vault_storage_and_a_control() {
    // No vault: nothing can be saved, and the view says so.
    let server = mock(200, probe_answer()).await;
    let state = base_state()
        .with_jev(Some(jev()))
        .with_judge_control(Some(control(&server.base)));
    let app = app!(state);
    let (status, body) = call!(app, put_key(KEY));
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("credential_store_unavailable"))
    );
    let (_, view) = call!(app, get());
    assert_eq!(view["available"], false);
    let refusal = control(&server.base)
        .save_key(&state, OpenAiApiKey::parse(KEY).expect("key"))
        .await
        .expect_err("no vault");
    assert_eq!(refusal.status, StatusCode::SERVICE_UNAVAILABLE);

    // A vault over failing storage refuses rather than half-applying.
    let failing = base_state()
        .with_runtime_state(RuntimeState::new(Some(Arc::new(FailingState))))
        .with_credential_vault(Some(test_vault()))
        .with_jev(Some(jev()))
        .with_judge_control(Some(control(&server.base)));
    let app = app!(failing);
    for request in [put_key(KEY), delete_key(), select("typesafe")] {
        let (status, body) = call!(app, request);
        assert_eq!(
            (status, body["error"].as_str()),
            (503, Some("judge_not_saved"))
        );
    }

    // A key that cannot be stored is not adopted, and a removal that cannot
    // be stored keeps the key, matching what a restart would read back.
    let picky = base_state()
        .with_runtime_state(RuntimeState::new(Some(Arc::new(KeyWritesFail::default()))))
        .with_credential_vault(Some(test_vault()))
        .with_jev(Some(jev()))
        .with_judge_control(Some(control(&server.base)));
    let app = app!(picky);
    let (status, body) = call!(app, put_key(KEY));
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("judge_not_saved"))
    );
    assert_eq!(call!(app, get()).1["openai"]["key"]["set"], false);
    let judge = picky.judge_control().expect("control");
    judge.update(|selection| selection.key = OpenAiApiKey::parse(KEY).ok());
    assert_eq!(call!(app, delete_key()).0, 503);
    assert_eq!(call!(app, get()).1["openai"]["key"]["set"], true);

    // Without a control the routes say so instead of guessing.
    let bare = base_state().with_credential_vault(Some(test_vault()));
    let app = app!(bare);
    assert_eq!(call!(app, get()).1["error"], "judge_selection_unavailable");
    for request in [select("typesafe"), put_key(KEY), delete_key(), run_test()] {
        assert_eq!(call!(app, request).0, 503);
    }
}

#[actix_web::test]
async fn the_selection_survives_a_restart() {
    let harness = harness(true).await;
    harness.server.reply(200, probe_answer());
    let app = app!(harness.state);
    assert_eq!(call!(app, put_key(KEY)).0, 200);
    assert_eq!(call!(app, run_test()).1["ok"], true);
    assert_eq!(call!(app, select("openai")).0, 200);

    let runtime_state = harness.state.runtime_state();
    let restarted = control(&harness.server.base);
    let runtime = jev();
    restarted
        .restore(runtime_state, &test_vault(), Some(&runtime))
        .await
        .expect("restores");
    assert_eq!(restarted.provider(), JevProvider::OpenAi);
    assert_eq!(runtime.provider(), JevProvider::OpenAi);
    let resumed = harness.state.clone().with_judge_control(Some(restarted));
    let view = resumed.judge_control().expect("control").view(&resumed);
    assert_eq!(view["openai"]["key"]["hint"], "WXYZ");
    assert_eq!(view["openai"]["test"]["ok"], true);

    // Without the Jev fallback the saved choice does not resume.
    let without_jev = control(&harness.server.base);
    without_jev
        .restore(runtime_state, &test_vault(), None)
        .await
        .expect("restores");
    assert_eq!(without_jev.provider(), JevProvider::TypeSafe);

    // A different configured model needs a new test.
    let other_model = control_with(&harness.server.base, Some("gpt-6-luna-mini"));
    let runtime = jev();
    other_model
        .restore(runtime_state, &test_vault(), Some(&runtime))
        .await
        .expect("restores");
    assert_eq!(other_model.provider(), JevProvider::TypeSafe);
    assert_eq!(runtime.provider(), JevProvider::TypeSafe);
    let resumed = harness.state.clone().with_judge_control(Some(other_model));
    let view = resumed.judge_control().expect("control").view(&resumed);
    assert_eq!(view["openai"]["test"], Value::Null);
    assert_eq!(view["openai"]["key"]["set"], true);
    assert_eq!(view["openai"]["model"], "gpt-6-luna-mini");
}

#[actix_web::test]
async fn restore_starts_clean_and_fails_loudly_on_unreadable_state() {
    let server = mock(403, NOT_ENABLED.to_owned()).await;
    let vault = test_vault();

    // Nothing stored: TypeSafe, no key.
    let store = Arc::new(MemoryState::default());
    let runtime_state = RuntimeState::new(Some(store.clone()));
    let fresh = control(&server.base);
    fresh
        .restore(&runtime_state, &vault, Some(&jev()))
        .await
        .expect("empty state restores");
    assert_eq!(fresh.provider(), JevProvider::TypeSafe);
    assert!(fresh.selection().key.is_none());

    // A key without prefs is adopted; a tombstone reads as no key.
    store
        .seed(
            StateKey::JudgeOpenAiKey,
            vault.seal_text(KEY).expect("seal"),
        )
        .await;
    let with_key = control(&server.base);
    with_key
        .restore(&runtime_state, &vault, None)
        .await
        .expect("restores");
    assert!(with_key.selection().key.is_some());
    store
        .seed(
            StateKey::JudgeOpenAiKey,
            json!({"version": 1, "ciphertext": null}),
        )
        .await;
    let tombstoned = control(&server.base);
    tombstoned
        .restore(&runtime_state, &vault, None)
        .await
        .expect("restores");
    assert!(tombstoned.selection().key.is_none());

    // A saved OpenAI choice without a key resumes as TypeSafe.
    store
        .seed(
            StateKey::JudgePrefs,
            json!({"provider": "openai", "test": null}),
        )
        .await;
    let keyless = control(&server.base);
    keyless
        .restore(&runtime_state, &vault, Some(&jev()))
        .await
        .expect("restores");
    assert_eq!(keyless.provider(), JevProvider::TypeSafe);

    // A saved OpenAI choice whose latest test failed resumes as TypeSafe.
    store
        .seed(
            StateKey::JudgeOpenAiKey,
            vault.seal_text(KEY).expect("seal"),
        )
        .await;
    store
        .seed(
            StateKey::JudgePrefs,
            json!({"provider": "openai", "test": {"ok": false, "atMs": 1, "latencyMs": 2, "detail": "no", "model": "gpt-6-luna"}}),
        )
        .await;
    let failed = control(&server.base);
    failed
        .restore(&runtime_state, &vault, Some(&jev()))
        .await
        .expect("restores");
    assert_eq!(failed.provider(), JevProvider::TypeSafe);

    // Unreadable prefs, an unknown provider, a malformed or undecryptable
    // key, and unreadable storage all fail startup.
    for (value, expected) in [
        (json!({"provider": "openai", "extra": 1}), "unreadable"),
        (json!({"provider": "gemini", "test": null}), "unknown"),
    ] {
        store.seed(StateKey::JudgePrefs, value).await;
        let error = control(&server.base)
            .restore(&runtime_state, &vault, None)
            .await
            .expect_err("must fail loudly");
        assert!(error.contains(expected), "{error}");
    }
    store
        .seed(
            StateKey::JudgePrefs,
            json!({"provider": "typesafe", "test": null}),
        )
        .await;
    for stored in [
        vault.seal_text("not-an-openai-key").expect("seal"),
        json!({"version": 1, "ciphertext": "AAAA"}),
    ] {
        store.seed(StateKey::JudgeOpenAiKey, stored).await;
        assert!(
            control(&server.base)
                .restore(&runtime_state, &vault, None)
                .await
                .is_err()
        );
    }
    for broken in [
        RuntimeState::new(Some(Arc::new(FailingState))),
        RuntimeState::disabled(),
    ] {
        assert!(
            control(&server.base)
                .restore(&broken, &vault, None)
                .await
                .is_err()
        );
    }
}
