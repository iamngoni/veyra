//! HTTP transport for TypeSafe's System One evaluation endpoint.
//!
//! One shared `reqwest` client with an explicit connect timeout and a total
//! request timeout performs `POST /v1/systemone`. The transport owns status
//! mapping and nothing else: request and response meaning live in
//! [`crate::jev::contract`], and every response is validated against the
//! request that produced it before a caller sees it.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use super::contract::{JevRequest, JevResponse, parse_response_body};
use super::settings::{JevApiKey, JevSettings};
use super::{JevError, JevProvider, SemanticJudge};

/// `POST /v1/systemone` client.
#[derive(Debug)]
pub struct HttpJev {
    client: reqwest::Client,
    api_key: JevApiKey,
    endpoint: String,
    model: String,
}

impl HttpJev {
    /// Builds the client from validated settings.
    ///
    /// # Errors
    /// Returns [`JevError::Transport`] when the HTTP client cannot be built.
    pub fn from_settings(settings: &JevSettings) -> Result<Self, JevError> {
        let client = reqwest::Client::builder()
            .connect_timeout(settings.connect_timeout())
            .timeout(settings.request_timeout())
            .build()
            .map_err(|error| JevError::Transport {
                reason: format!("http client construction failed: {error}"),
            })?;
        Ok(Self {
            client,
            api_key: settings.api_key().clone(),
            endpoint: format!("{}/v1/systemone", settings.base_url()),
            model: settings.model().to_owned(),
        })
    }
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    state: &'a Value,
    model: &'a str,
    questions: BTreeMap<&'a str, Value>,
}

#[async_trait]
impl SemanticJudge for HttpJev {
    fn provider(&self) -> JevProvider {
        JevProvider::TypeSafe
    }

    async fn judge(&self, request: JevRequest) -> Result<JevResponse, JevError> {
        let questions = request
            .questions()
            .iter()
            .map(|(id, question)| (id.as_str(), question.wire()))
            .collect();
        let body = serde_json::to_value(WireRequest {
            state: request.state().value(),
            model: &self.model,
            questions,
        })
        .map_err(|error| JevError::Transport {
            reason: format!("request encoding failed: {error}"),
        })?;

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|error| JevError::Transport {
                reason: transport_reason(&error),
            })?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| JevError::Transport {
                reason: transport_reason(&error),
            })?;

        match status {
            200..=299 => {}
            401 | 403 => return Err(JevError::Unauthorized),
            422 => {
                return Err(JevError::Rejected {
                    detail: error_detail(&bytes),
                });
            }
            429 | 529 => return Err(JevError::Unavailable { status }),
            other => {
                return Err(JevError::Transport {
                    reason: format!("unexpected status {other}"),
                });
            }
        }

        // A rejected response is logged with a bounded preview: provider drift
        // in probability formatting must be diagnosable without reconstructing
        // the request, and the response carries only judgements.
        let parsed = match parse_response_body(&bytes) {
            Ok(parsed) => parsed,
            Err(error) => {
                logging::warn_rejected(&error, &bytes);
                return Err(error);
            }
        };
        if let Err(error) = request.validate_answers(&parsed) {
            logging::warn_rejected(&error, &bytes);
            return Err(error);
        }
        Ok(parsed)
    }
}

mod logging {
    use crate::jev::JevError;

    /// Bounded response preview attached to contract rejections.
    const PREVIEW_CHARS: usize = 700;

    pub(super) fn warn_rejected(error: &JevError, bytes: &[u8]) {
        let preview: String = String::from_utf8_lossy(bytes)
            .chars()
            .filter(|character| !character.is_control() || *character == ' ')
            .take(PREVIEW_CHARS)
            .collect();
        tracing::warn!(%error, preview, "jev response rejected");
    }
}

/// Classifies a transport failure without echoing request bodies.
fn transport_reason(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_decode() {
        "decode"
    } else {
        "request"
    };
    format!("{kind} error: {error}")
}

/// Keeps the first 256 printable characters of an error body.
fn error_detail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let cleaned: String = text.chars().filter(|c| !c.is_control()).take(256).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "request rejected without detail".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use actix_web::http::StatusCode as HttpStatus;
    use actix_web::{App, HttpRequest, HttpResponse, HttpServer, post, web};
    use serde_json::json;

    use super::*;
    use crate::config::ConfigError;
    use crate::jev::contract::{
        ChoiceOptions, Instructions, JevRequest, NoulCriteria, Question, ScoreLevels, State,
    };

    const KEY: &str = "apikey_test_1234567890";

    #[derive(Debug)]
    struct TestState {
        reply: Mutex<(u16, String)>,
        requests: Mutex<Vec<Value>>,
        authorization: Mutex<Option<String>>,
    }

    #[post("/v1/systemone")]
    async fn systemone(
        request: HttpRequest,
        body: web::Bytes,
        state: web::Data<Arc<TestState>>,
    ) -> HttpResponse {
        if let Some(value) = request
            .headers()
            .get(actix_web::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        {
            *state.authorization.lock().expect("lock") = Some(value.to_owned());
        }
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        state.requests.lock().expect("lock").push(parsed);
        let (status, payload) = state.reply.lock().expect("lock").clone();
        HttpResponse::build(HttpStatus::from_u16(status).expect("status")).body(payload)
    }

    struct TestServer {
        handle: actix_web::dev::ServerHandle,
        base_url: String,
        state: Arc<TestState>,
    }

    impl TestServer {
        async fn shutdown(self) {
            self.handle.stop(true).await;
        }
    }

    async fn spawn(reply: (u16, String)) -> TestServer {
        let state = Arc::new(TestState {
            reply: Mutex::new(reply),
            requests: Mutex::new(Vec::new()),
            authorization: Mutex::new(None),
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let server = HttpServer::new({
            let state = state.clone();
            move || {
                App::new()
                    .app_data(web::Data::new(state.clone()))
                    .service(systemone)
            }
        })
        .listen(listener)
        .expect("listen")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);
        TestServer {
            handle,
            base_url: format!("http://{address}"),
            state,
        }
    }

    fn settings(base_url: &str) -> JevSettings {
        JevSettings::from_source(|name| match name {
            "VEYRA_JEV_API_KEY" => Ok(KEY.to_owned()),
            "VEYRA_JEV_BASE_URL" => Ok(base_url.to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured")
    }

    fn request() -> JevRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "direction".to_owned(),
            Question::choice(
                Instructions::text("Which direction?").expect("instructions"),
                ChoiceOptions::new([
                    ("long".to_owned(), None),
                    ("short".to_owned(), None),
                    ("flat".to_owned(), None),
                ])
                .expect("options"),
            ),
        );
        questions.insert(
            "is_trending".to_owned(),
            Question::noul(
                Instructions::text("Is it trending?").expect("instructions"),
                NoulCriteria::default(),
            ),
        );
        questions.insert(
            "momentum".to_owned(),
            Question::score(
                Instructions::text("How strong?").expect("instructions"),
                ScoreLevels::new(["Weak".to_owned(), "Neutral".to_owned(), "Strong".to_owned()])
                    .expect("levels"),
            ),
        );
        JevRequest::new(
            State::text("EURUSD closed above its average.").expect("state"),
            questions,
        )
        .expect("request")
    }

    fn success_body() -> String {
        json!({
            "model": "jev-1.13.0",
            "answers": {
                "direction": {"type": "choice", "choice": "long", "probabilities": {"long": 0.99, "flat": 0.0, "short": 0.01}, "confidence": 0.98},
                "is_trending": {"type": "noul", "noul": 0.78},
                "momentum": {"type": "score", "score": 1.96, "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"}, "probabilities": {"0": 0.01, "1": 0.03, "2": 0.96}, "confidence": 0.94}
            },
            "usage": {"input_tokens": 402, "output_tokens": 73}
        })
        .to_string()
    }

    #[actix_web::test]
    async fn judge_sends_the_documented_request_and_returns_typed_answers() {
        let server = spawn((200, success_body())).await;
        let judge = HttpJev::from_settings(&settings(&server.base_url)).expect("client");

        let response = judge.judge(request()).await.expect("successful judgement");
        assert_eq!(response.model(), "jev-1.13.0");
        assert_eq!(response.usage().output_tokens, 73);
        assert!(matches!(
            response.answer("direction"),
            Some(crate::jev::Answer::Choice(_))
        ));

        {
            let captured = server.state.requests.lock().expect("lock");
            let body = captured.first().expect("one request");
            assert_eq!(body["model"], "jev-latest");
            assert_eq!(body["state"], "EURUSD closed above its average.");
            assert_eq!(body["questions"]["direction"]["type"], "choice");
            assert_eq!(body["questions"]["momentum"]["criteria"][2], "Strong");
        }
        assert_eq!(
            server.state.authorization.lock().expect("lock").as_deref(),
            Some("Bearer apikey_test_1234567890")
        );

        server.shutdown().await;
    }

    #[actix_web::test]
    async fn status_codes_map_to_typed_errors() {
        let unauthorized = spawn((401, String::new())).await;
        let judge = HttpJev::from_settings(&settings(&unauthorized.base_url)).expect("client");
        assert!(matches!(
            judge.judge(request()).await.expect_err("401"),
            JevError::Unauthorized
        ));
        unauthorized.shutdown().await;

        let rejected = spawn((
            422,
            json!({"detail": "questions.direction.criteria: required"}).to_string(),
        ))
        .await;
        let judge = HttpJev::from_settings(&settings(&rejected.base_url)).expect("client");
        match judge.judge(request()).await.expect_err("422") {
            JevError::Rejected { detail } => assert!(detail.contains("criteria")),
            other => panic!("unexpected error {other:?}"),
        }
        rejected.shutdown().await;

        let empty_rejection = spawn((422, String::new())).await;
        let judge = HttpJev::from_settings(&settings(&empty_rejection.base_url)).expect("client");
        match judge.judge(request()).await.expect_err("422") {
            JevError::Rejected { detail } => {
                assert_eq!(detail, "request rejected without detail")
            }
            other => panic!("unexpected error {other:?}"),
        }
        empty_rejection.shutdown().await;

        for status in [429_u16, 529] {
            let overloaded = spawn((status, String::new())).await;
            let judge = HttpJev::from_settings(&settings(&overloaded.base_url)).expect("client");
            match judge.judge(request()).await.expect_err("backoff") {
                JevError::Unavailable { status: reported } => assert_eq!(reported, status),
                other => panic!("unexpected error {other:?}"),
            }
            overloaded.shutdown().await;
        }

        let broken = spawn((500, "internal".to_owned())).await;
        let judge = HttpJev::from_settings(&settings(&broken.base_url)).expect("client");
        match judge.judge(request()).await.expect_err("500") {
            JevError::Transport { reason } => assert!(reason.contains("500")),
            other => panic!("unexpected error {other:?}"),
        }
        broken.shutdown().await;
    }

    #[actix_web::test]
    async fn malformed_or_misaligned_responses_are_rejected() {
        let not_json = spawn((200, "not json".to_owned())).await;
        let judge = HttpJev::from_settings(&settings(&not_json.base_url)).expect("client");
        assert!(matches!(
            judge.judge(request()).await.expect_err("bad json"),
            JevError::MalformedResponse { .. }
        ));
        not_json.shutdown().await;

        let misaligned = spawn((
            200,
            json!({
                "model": "jev-1.13.0",
                "answers": {
                    "direction": {"type": "choice", "choice": "sideways", "probabilities": {"sideways": 0.6, "long": 0.4}, "confidence": 0.5},
                    "is_trending": {"type": "noul", "noul": 0.5},
                    "momentum": {"type": "score", "score": 1.0, "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"}, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}, "confidence": 0.5}
                },
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })
            .to_string(),
        ))
        .await;
        let judge = HttpJev::from_settings(&settings(&misaligned.base_url)).expect("client");
        assert!(matches!(
            judge.judge(request()).await.expect_err("misaligned"),
            JevError::Contract { .. }
        ));
        misaligned.shutdown().await;
    }

    #[actix_web::test]
    async fn unreachable_endpoints_report_transport_failures() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        drop(listener);

        let judge =
            HttpJev::from_settings(&settings(&format!("http://{address}"))).expect("client");
        match judge.judge(request()).await.expect_err("refused") {
            JevError::Transport { reason } => assert!(reason.contains("connect")),
            other => panic!("unexpected error {other:?}"),
        }
    }
}
