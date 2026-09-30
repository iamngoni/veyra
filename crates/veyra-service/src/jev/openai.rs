//! OpenAI Decisions transport: `POST {base}/v1/decisions`.
//!
//! Boundary: OpenAI's Decisions API is a limited preview without public
//! documentation. A live call on 2026-09-30 verified only that the endpoint
//! exists, takes a bearer API key, and answers
//! `403 {"error":{"message":"Decision API is not enabled for this user."}}`
//! for accounts outside the preview. The request and response bodies are
//! **assumed** to match TypeSafe's System One shape, so this module reuses
//! that wire encoding and the same strict response validation: any
//! difference surfaces as a contract error, never as a guessed answer.
//!
//! This judge never answers alone for the trade pipeline. It runs either as
//! the primary of a [`FallbackJudge`](super::fallback::FallbackJudge) over the
//! configured TypeSafe judge, or in the console's explicit connection test.
//!
//! One shared `reqwest` client with explicit connect and total timeouts is
//! built once at startup ([`OpenAiDecisions`]); a judge built for a key clones
//! its handle, which shares the connection pool. Keys are redacted from
//! `Debug` output, never logged, and scrubbed from provider error details.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::contract::{JevRequest, JevResponse};
use super::http::{accept_response, error_detail, post_json, wire_body};
use super::settings::{base_url, model_name, optional};
use super::{JevError, JevProvider, SemanticJudge};
use crate::config::ConfigError;

/// OpenAI API origin used when `VEYRA_JUDGE_OPENAI_BASE_URL` is empty.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";
/// Decisions model used when `VEYRA_JUDGE_OPENAI_MODEL` is empty.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

const KEY_PREFIX: &str = "sk-";
const MIN_KEY_CHARS: usize = 20;
const MAX_KEY_CHARS: usize = 512;
/// Longest provider error detail kept for logs and the console.
const MAX_DETAIL_CHARS: usize = 256;

/// Why a candidate OpenAI key was refused. Never carries the key itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpenAiKeyError {
    /// The key does not start with `sk-`.
    #[error("OpenAI API keys start with sk-")]
    Prefix,
    /// The key is shorter than 20 or longer than 512 characters.
    #[error("OpenAI API keys are 20-512 characters long")]
    Length,
    /// The key contains characters outside letters, digits, '-' and '_'.
    #[error("OpenAI API keys contain only letters, digits, '-' and '_'")]
    Characters,
}

/// OpenAI API key; `Debug` output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiApiKey(String);

impl OpenAiApiKey {
    /// Parses a trimmed key: `sk-` prefix, 20-512 characters of letters,
    /// digits, '-' or '_' (covers project, service-account and legacy keys).
    ///
    /// # Errors
    /// Returns the [`OpenAiKeyError`] naming the first violated rule.
    pub fn parse(value: &str) -> Result<Self, OpenAiKeyError> {
        let trimmed = value.trim();
        if !trimmed.starts_with(KEY_PREFIX) {
            return Err(OpenAiKeyError::Prefix);
        }
        if !(MIN_KEY_CHARS..=MAX_KEY_CHARS).contains(&trimmed.len()) {
            return Err(OpenAiKeyError::Length);
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(OpenAiKeyError::Characters);
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Exposes the secret for the authorization header and sealing.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Last four characters, for "is this the key I saved?" checks.
    pub fn hint(&self) -> String {
        // Keys are ASCII and at least 20 characters, so this is a char
        // boundary and never most of the key.
        self.0
            .get(self.0.len().saturating_sub(4)..)
            .unwrap_or_default()
            .to_owned()
    }
}

impl fmt::Debug for OpenAiApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenAiApiKey(redacted)")
    }
}

/// Non-secret OpenAI Decisions settings read from the environment.
///
/// `VEYRA_JUDGE_OPENAI_MODEL` and `VEYRA_JUDGE_OPENAI_BASE_URL` are both
/// optional; the key itself is entered in the console and sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiSettings {
    base_url: String,
    model: String,
}

impl Default for OpenAiSettings {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
        }
    }
}

impl OpenAiSettings {
    /// Reads settings from the process environment.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for a malformed model or base URL.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|name| {
            std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
        })
    }

    /// Parses an injected settings source; empty values use the defaults.
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidEnvironmentVariable`] naming the
    /// malformed variable.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let base_raw = optional(&mut source, "VEYRA_JUDGE_OPENAI_BASE_URL");
        let model_raw = optional(&mut source, "VEYRA_JUDGE_OPENAI_MODEL");
        let defaults = Self::default();
        Ok(Self {
            base_url: if base_raw.is_empty() {
                defaults.base_url
            } else {
                base_url("VEYRA_JUDGE_OPENAI_BASE_URL", &base_raw)?
            },
            model: if model_raw.is_empty() {
                defaults.model
            } else {
                model_name("VEYRA_JUDGE_OPENAI_MODEL", &model_raw)?
            },
        })
    }

    /// Base URL without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Decisions model sent with every request.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Connect timeout for the shared HTTP client.
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }

    /// Total request timeout. Shorter than Jev's: a hung primary delays the
    /// fallback answer by this much, so it is bounded tightly.
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(15)
    }
}

/// Shared `POST /v1/decisions` client, built once at startup.
#[derive(Debug, Clone)]
pub struct OpenAiDecisions {
    client: reqwest::Client,
    endpoint: String,
    model: String,
}

impl OpenAiDecisions {
    /// Builds the shared client from validated settings.
    ///
    /// # Errors
    /// Returns [`JevError::Transport`] when the HTTP client cannot be built.
    pub fn new(settings: &OpenAiSettings) -> Result<Self, JevError> {
        let client = reqwest::Client::builder()
            .connect_timeout(settings.connect_timeout())
            .timeout(settings.request_timeout())
            .build()
            .map_err(|error| JevError::Transport {
                reason: format!("http client construction failed: {error}"),
            })?;
        Ok(Self {
            client,
            endpoint: format!("{}/v1/decisions", settings.base_url()),
            model: settings.model().to_owned(),
        })
    }

    /// Decisions model sent with every request.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// A judge that authenticates with `key` over the shared client.
    pub fn judge(&self, key: OpenAiApiKey) -> OpenAiJudge {
        OpenAiJudge {
            decisions: self.clone(),
            key,
        }
    }
}

/// OpenAI Decisions judge for one API key.
#[derive(Debug)]
pub struct OpenAiJudge {
    decisions: OpenAiDecisions,
    key: OpenAiApiKey,
}

#[async_trait]
impl SemanticJudge for OpenAiJudge {
    fn provider(&self) -> JevProvider {
        JevProvider::OpenAi
    }

    async fn judge(&self, request: JevRequest) -> Result<JevResponse, JevError> {
        let body = wire_body(&request, &self.decisions.model)?;
        let (status, bytes) = post_json(
            &self.decisions.client,
            &self.decisions.endpoint,
            self.key.expose(),
            &body,
        )
        .await?;
        match status {
            200..=299 => accept_response(JevProvider::OpenAi, &request, &bytes),
            // 403 is also how the limited preview says "not enabled for this
            // user", so the provider's own message is carried through.
            401 | 403 => Err(JevError::Denied {
                status,
                detail: provider_detail(&bytes),
            }),
            400 | 404 | 422 => Err(JevError::Rejected {
                detail: provider_detail(&bytes),
            }),
            429 | 503 | 529 => Err(JevError::Unavailable { status }),
            other => Err(JevError::Transport {
                reason: format!("unexpected status {other}: {}", provider_detail(&bytes)),
            }),
        }
    }
}

/// OpenAI's `error.message` when the body carries one, else a bounded body
/// preview; anything shaped like an API key is scrubbed either way.
fn provider_detail(bytes: &[u8]) -> String {
    let message = serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|body| {
            body.pointer("/error/message")
                .and_then(Value::as_str)
                .map(|text| {
                    text.chars()
                        .filter(|c| !c.is_control())
                        .take(MAX_DETAIL_CHARS)
                        .collect::<String>()
                })
        })
        .filter(|text| !text.trim().is_empty());
    let detail = match message {
        Some(text) => text.trim().to_owned(),
        None => error_detail(bytes),
    };
    redact_keys(&detail)
}

/// Replaces every whitespace-separated word that looks like an API key.
fn redact_keys(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            let bare = word.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
            if bare.starts_with(KEY_PREFIX) {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::jev::contract::{
        ChoiceOptions, Instructions, NoulCriteria, Question, ScoreLevels, State,
    };

    pub(crate) const KEY: &str = "sk-proj-test_0123456789abcdWXYZ";

    #[test]
    fn keys_are_validated_without_echoing_them() {
        let key = OpenAiApiKey::parse(&format!("  {KEY}\n")).expect("valid key");
        assert_eq!(key.expose(), KEY);
        assert_eq!(key.hint(), "WXYZ");
        assert_eq!(format!("{key:?}"), "OpenAiApiKey(redacted)");

        assert_eq!(
            OpenAiApiKey::parse("pk-0123456789abcdefghijkl"),
            Err(OpenAiKeyError::Prefix)
        );
        assert_eq!(OpenAiApiKey::parse("sk-short"), Err(OpenAiKeyError::Length));
        assert_eq!(
            OpenAiApiKey::parse(&format!("sk-{}", "a".repeat(510))),
            Err(OpenAiKeyError::Length)
        );
        assert_eq!(
            OpenAiApiKey::parse("sk-0123456789 abcdefghijkl"),
            Err(OpenAiKeyError::Characters)
        );
        for error in [
            OpenAiKeyError::Prefix,
            OpenAiKeyError::Length,
            OpenAiKeyError::Characters,
        ] {
            assert!(error.to_string().starts_with("OpenAI API keys"));
        }
    }

    fn source<'a>(
        pairs: &'a [(&'static str, &'a str)],
    ) -> impl FnMut(&'static str) -> Result<String, ConfigError> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(ConfigError::MissingEnvironmentVariable { name })
        }
    }

    #[test]
    fn settings_default_and_override() {
        let defaults = OpenAiSettings::from_source(source(&[])).expect("defaults");
        assert_eq!(defaults, OpenAiSettings::default());
        assert_eq!(defaults.base_url(), "https://api.openai.com");
        assert_eq!(defaults.model(), "gpt-6-luna");
        assert_eq!(defaults.connect_timeout(), Duration::from_secs(5));
        assert_eq!(defaults.request_timeout(), Duration::from_secs(15));

        let custom = OpenAiSettings::from_source(source(&[
            ("VEYRA_JUDGE_OPENAI_BASE_URL", "http://127.0.0.1:9000/"),
            ("VEYRA_JUDGE_OPENAI_MODEL", "gpt-6-luna-2026-09"),
        ]))
        .expect("custom");
        assert_eq!(custom.base_url(), "http://127.0.0.1:9000");
        assert_eq!(custom.model(), "gpt-6-luna-2026-09");

        for (name, value) in [
            ("VEYRA_JUDGE_OPENAI_BASE_URL", "http://api.openai.com"),
            ("VEYRA_JUDGE_OPENAI_MODEL", "bad model"),
        ] {
            match OpenAiSettings::from_source(source(&[(name, value)])) {
                Err(ConfigError::InvalidEnvironmentVariable { name: reported, .. }) => {
                    assert_eq!(reported, name)
                }
                other => panic!("{name}={value} must be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn provider_details_prefer_the_error_message_and_scrub_keys() {
        let body = json!({"error": {"message": "Decision API is not enabled for this user.", "type": "invalid_request_error"}});
        assert_eq!(
            provider_detail(body.to_string().as_bytes()),
            "Decision API is not enabled for this user."
        );
        let leaky = json!({"error": {"message": "Incorrect API key provided: sk-proj-abc****WXYZ. See docs."}});
        assert_eq!(
            provider_detail(leaky.to_string().as_bytes()),
            "Incorrect API key provided: [redacted] See docs."
        );
        assert_eq!(provider_detail(b"plain failure"), "plain failure");
        assert_eq!(
            provider_detail(json!({"error": {"message": "  "}}).to_string().as_bytes()),
            "{\"error\":{\"message\":\"  \"}}"
        );
        assert_eq!(provider_detail(b""), "request rejected without detail");
        assert_eq!(
            redact_keys("risk-free (sk-live_123)"),
            "risk-free [redacted]"
        );
    }

    /// Local stand-in for the Decisions endpoint.
    pub(crate) struct Mock {
        /// Base URL to configure the client with.
        pub(crate) base: String,
        /// Every request's head and body, in arrival order.
        pub(crate) requests: Arc<Mutex<Vec<(String, String)>>>,
        reply: Arc<Mutex<(u16, String)>>,
    }

    impl Mock {
        /// Changes what later requests are answered with.
        pub(crate) fn reply(&self, status: u16, body: String) {
            *self.reply.lock().expect("reply") = (status, body);
        }
    }

    /// Answers every request with the current reply, recording each request.
    pub(crate) async fn mock(status: u16, body: String) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new((status, body)));
        let record = requests.clone();
        let current = reply.clone();
        actix_web::rt::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut raw = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if rest.len() >= length {
                            record
                                .lock()
                                .expect("requests")
                                .push((head.to_owned(), rest.to_owned()));
                            break;
                        }
                    }
                }
                let (status, body) = current.lock().expect("reply").clone();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        Mock {
            base,
            requests,
            reply,
        }
    }

    pub(crate) fn decisions(base_url: &str) -> OpenAiDecisions {
        let settings = OpenAiSettings::from_source(|name| match name {
            "VEYRA_JUDGE_OPENAI_BASE_URL" => Ok(base_url.to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings");
        OpenAiDecisions::new(&settings).expect("client")
    }

    fn request() -> JevRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "direction".to_owned(),
            Question::choice(
                Instructions::text("Which direction?").expect("instructions"),
                ChoiceOptions::new([("long".to_owned(), None), ("short".to_owned(), None)])
                    .expect("options"),
            ),
        );
        questions.insert(
            "trending".to_owned(),
            Question::noul(
                Instructions::text("Trending?").expect("instructions"),
                NoulCriteria::default(),
            ),
        );
        questions.insert(
            "momentum".to_owned(),
            Question::score(
                Instructions::text("How strong?").expect("instructions"),
                ScoreLevels::new(["Weak".to_owned(), "Strong".to_owned()]).expect("levels"),
            ),
        );
        JevRequest::new(State::text("EURUSD rose.").expect("state"), questions).expect("request")
    }

    /// A System One-shaped answer to [`request`].
    pub(crate) fn success_body() -> String {
        json!({
            "id": "dec_123",
            "model": "gpt-6-luna-2026-09-01",
            "answers": {
                "direction": {"type": "choice", "choice": "long", "probabilities": {"long": 0.8, "short": 0.2}, "confidence": 0.7},
                "trending": {"type": "noul", "noul": 0.66},
                "momentum": {"type": "score", "score": 0.9, "legend": {"0": "Weak", "1": "Strong"}, "probabilities": {"0": 0.1, "1": 0.9}, "confidence": 0.8}
            },
            "usage": {"input_tokens": 120, "output_tokens": 9}
        })
        .to_string()
    }

    #[actix_web::test]
    async fn posts_the_system_one_shape_to_the_decisions_endpoint() {
        let server = mock(200, success_body()).await;
        let decisions = decisions(&server.base);
        assert_eq!(decisions.model(), "gpt-6-luna");
        let judge = decisions.judge(OpenAiApiKey::parse(KEY).expect("key"));
        assert_eq!(judge.provider(), JevProvider::OpenAi);
        assert!(!format!("{judge:?}").contains(KEY));

        let response = judge.judge(request()).await.expect("answers");
        assert_eq!(response.model(), "gpt-6-luna-2026-09-01");
        assert_eq!(response.usage().input_tokens, 120);

        let requests = server.requests.lock().expect("requests").clone();
        let (head, body) = requests.first().expect("one request");
        assert!(head.starts_with("POST /v1/decisions HTTP/1.1"));
        assert!(head.to_ascii_lowercase().contains(&format!(
            "authorization: bearer {}",
            KEY.to_ascii_lowercase()
        )));
        let body: Value = serde_json::from_str(body).expect("json body");
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["state"], "EURUSD rose.");
        assert_eq!(body["questions"]["direction"]["type"], "choice");
        assert_eq!(body["questions"]["trending"]["type"], "noul");
        assert_eq!(body["questions"]["momentum"]["criteria"][1], "Strong");
    }

    async fn failure(status: u16, body: &str) -> JevError {
        let server = mock(status, body.to_owned()).await;
        decisions(&server.base)
            .judge(OpenAiApiKey::parse(KEY).expect("key"))
            .judge(request())
            .await
            .expect_err("must fail")
    }

    #[actix_web::test]
    async fn not_enabled_surfaces_the_provider_message() {
        let body = json!({"error": {"message": "Decision API is not enabled for this user.", "type": "invalid_request_error", "code": null}}).to_string();
        match failure(403, &body).await {
            JevError::Denied { status, detail } => {
                assert_eq!(status, 403);
                assert_eq!(detail, "Decision API is not enabled for this user.");
            }
            other => panic!("unexpected {other:?}"),
        }
        let error = failure(
            401,
            r#"{"error":{"message":"Incorrect API key provided: sk-proj-****WXYZ."}}"#,
        )
        .await;
        assert!(matches!(error, JevError::Denied { status: 401, .. }));
        assert!(!error.to_string().contains("sk-proj"));
        assert!(error.to_string().contains("401"));
    }

    #[actix_web::test]
    async fn statuses_and_malformed_bodies_fail_closed() {
        match failure(
            404,
            r#"{"error":{"message":"The model `gpt-6-luna` does not exist"}}"#,
        )
        .await
        {
            JevError::Rejected { detail } => assert!(detail.contains("does not exist")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            failure(429, "{}").await,
            JevError::Unavailable { status: 429 }
        ));
        match failure(500, "boom").await {
            JevError::Transport { reason } => assert_eq!(reason, "unexpected status 500: boom"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            failure(200, "not json").await,
            JevError::MalformedResponse { .. }
        ));
        // A well-formed body in a different shape (a chat completion, say) is
        // a contract failure, never a guessed answer.
        assert!(matches!(
            failure(
                200,
                r#"{"id":"x","choices":[{"message":{"content":"long"}}]}"#
            )
            .await,
            JevError::MalformedResponse { .. }
        ));
        let misaligned = success_body().replace("\"trending\"", "\"other\"");
        assert!(matches!(
            failure(200, &misaligned).await,
            JevError::Contract { .. }
        ));
    }
}
