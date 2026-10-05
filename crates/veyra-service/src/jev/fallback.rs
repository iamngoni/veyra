//! Primary judge with an automatic fallback.
//!
//! Wraps an operator-selected primary (OpenAI Decisions) over the configured
//! judge (TypeSafe Jev). Any primary error — transport, status, or a response
//! that fails the contract — is logged and the same request goes to the
//! fallback, so a judgement round is never skipped because of the primary.
//! After a primary failure the primary is bypassed for a short window, so an
//! outage costs one timeout rather than one per instrument per tick. Every
//! request routed to the fallback is counted in [`JevUsage`].
//!
//! The fallback's own errors are returned unchanged: a failing configured
//! judge behaves exactly as it does without a primary.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::contract::{JevRequest, JevResponse};
use super::{JevError, JevProvider, JevUsage, SemanticJudge};

/// Primary-with-fallback [`SemanticJudge`].
#[derive(Debug)]
pub struct FallbackJudge {
    primary: Arc<dyn SemanticJudge>,
    fallback: Arc<dyn SemanticJudge>,
    usage: Arc<JevUsage>,
    retry_after: Duration,
    bypass_until: Mutex<Option<Instant>>,
}

impl FallbackJudge {
    /// How long the primary is skipped after it fails.
    pub const RETRY_AFTER: Duration = Duration::from_secs(60);

    /// Wraps `primary` over `fallback`, counting fallbacks in `usage`.
    pub(crate) fn new(
        primary: Arc<dyn SemanticJudge>,
        fallback: Arc<dyn SemanticJudge>,
        usage: Arc<JevUsage>,
        retry_after: Duration,
    ) -> Self {
        Self {
            primary,
            fallback,
            usage,
            retry_after,
            bypass_until: Mutex::new(None),
        }
    }

    /// Whether the primary is inside its post-failure bypass window.
    fn bypassing(&self) -> bool {
        let until = *self
            .bypass_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        until.is_some_and(|until| Instant::now() < until)
    }

    fn trip(&self) {
        *self
            .bypass_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now().checked_add(self.retry_after);
    }
}

#[async_trait]
impl SemanticJudge for FallbackJudge {
    fn provider(&self) -> JevProvider {
        self.primary.provider()
    }

    async fn judge(&self, request: JevRequest) -> Result<JevResponse, JevError> {
        if !self.bypassing() {
            match self.primary.judge(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    tracing::warn!(
                        primary = self.primary.provider().as_str(),
                        fallback = self.fallback.provider().as_str(),
                        %error,
                        retry_after_secs = self.retry_after.as_secs(),
                        "primary judge failed; answering with the fallback"
                    );
                    self.trip();
                }
            }
        }
        self.usage.fallbacks.fetch_add(1, Ordering::Relaxed);
        self.fallback.judge(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::jev::contract::{Instructions, NoulCriteria, Question, State};

    /// Scripted judge: fails while `fail` is set, counting every call.
    #[derive(Debug)]
    struct Scripted {
        provider: JevProvider,
        fail: bool,
        calls: AtomicUsize,
    }

    impl Scripted {
        fn new(provider: JevProvider, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                provider,
                fail,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SemanticJudge for Scripted {
        fn provider(&self) -> JevProvider {
            self.provider
        }

        async fn judge(&self, _request: JevRequest) -> Result<JevResponse, JevError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(JevError::Denied {
                    status: 403,
                    detail: "Decision API is not enabled for this user.".to_owned(),
                });
            }
            let body = serde_json::json!({
                "model": self.provider.as_str(),
                "answers": {"trending": {"type": "noul", "noul": 0.5}},
                "usage": {"input_tokens": 1, "output_tokens": 1}
            });
            crate::jev::contract::parse_response_body(body.to_string().as_bytes())
        }
    }

    fn request() -> JevRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "trending".to_owned(),
            Question::noul(
                Instructions::text("Trending?").expect("instructions"),
                NoulCriteria::default(),
            ),
        );
        JevRequest::new(State::text("probe").expect("state"), questions).expect("request")
    }

    #[actix_web::test]
    async fn a_healthy_primary_answers_without_touching_the_fallback() {
        let primary = Scripted::new(JevProvider::OpenAi, false);
        let fallback = Scripted::new(JevProvider::TypeSafe, false);
        let usage = Arc::new(JevUsage::default());
        let judge = FallbackJudge::new(
            primary.clone(),
            fallback.clone(),
            usage.clone(),
            FallbackJudge::RETRY_AFTER,
        );
        assert_eq!(judge.provider(), JevProvider::OpenAi);
        let answer = judge.judge(request()).await.expect("answers");
        assert_eq!(answer.model(), "openai");
        assert_eq!((primary.calls(), fallback.calls()), (1, 0));
        assert_eq!(usage.fallbacks.load(Ordering::Relaxed), 0);
    }

    #[actix_web::test]
    async fn a_failing_primary_falls_back_then_is_bypassed_for_a_while() {
        let primary = Scripted::new(JevProvider::OpenAi, true);
        let fallback = Scripted::new(JevProvider::TypeSafe, false);
        let usage = Arc::new(JevUsage::default());
        let judge = FallbackJudge::new(
            primary.clone(),
            fallback.clone(),
            usage.clone(),
            Duration::from_secs(3_600),
        );

        let answer = judge.judge(request()).await.expect("fallback answers");
        assert_eq!(answer.model(), "typesafe");
        assert_eq!((primary.calls(), fallback.calls()), (1, 1));

        // Inside the bypass window the primary is not asked again.
        judge.judge(request()).await.expect("fallback answers");
        assert_eq!((primary.calls(), fallback.calls()), (1, 2));
        assert_eq!(usage.fallbacks.load(Ordering::Relaxed), 2);
    }

    #[actix_web::test]
    async fn the_primary_is_retried_once_the_window_passes() {
        let primary = Scripted::new(JevProvider::OpenAi, true);
        let fallback = Scripted::new(JevProvider::TypeSafe, false);
        let judge = FallbackJudge::new(
            primary.clone(),
            fallback.clone(),
            Arc::new(JevUsage::default()),
            Duration::ZERO,
        );
        judge.judge(request()).await.expect("fallback answers");
        judge.judge(request()).await.expect("fallback answers");
        assert_eq!((primary.calls(), fallback.calls()), (2, 2));
    }

    #[actix_web::test]
    async fn a_failing_fallback_error_is_returned_unchanged() {
        let judge = FallbackJudge::new(
            Scripted::new(JevProvider::OpenAi, true),
            Scripted::new(JevProvider::TypeSafe, true),
            Arc::new(JevUsage::default()),
            FallbackJudge::RETRY_AFTER,
        );
        assert!(matches!(
            judge.judge(request()).await,
            Err(JevError::Denied { status: 403, .. })
        ));
    }
}
