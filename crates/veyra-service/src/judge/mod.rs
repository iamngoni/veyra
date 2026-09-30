//! Operator choice of semantic judge: TypeSafe Jev (the default) or OpenAI
//! Decisions with Jev as its automatic fallback.
//!
//! Safety and lifecycle boundaries:
//!
//! * The configured TypeSafe judge (`VEYRA_JEV_*`) is always the base. OpenAI
//!   is only ever layered over it as a primary through
//!   [`JevRuntime::use_primary`], so any OpenAI failure is answered by Jev.
//!   Without a configured Jev the OpenAI option is unavailable.
//! * Selecting OpenAI requires a saved key **and** a passing connection test
//!   of that key against the configured model. Saving or removing a key, or a
//!   failing test, returns the selection to TypeSafe.
//! * The key is sealed by the [`CredentialVault`] before storage, is never
//!   returned, and is shown only as a four-character hint.
//! * The selection and the latest test result are non-secret runtime state.
//!   Each change is saved before it is applied, and [`JudgeControl::restore`]
//!   resumes it at startup; unreadable state fails startup instead of
//!   silently changing the judge.
//! * One change runs at a time (a test included), so a test result always
//!   belongs to the key it tested.
//! * Judgements stay advisory inputs; nothing here can place or change an
//!   order.

pub mod routes;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use actix_web::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;
use crate::credential::CredentialVault;
use crate::jev::{
    ChoiceOptions, Instructions, JevError, JevProvider, JevRequest, JevRuntime, NoulCriteria,
    OpenAiApiKey, OpenAiDecisions, OpenAiSettings, Question, ScoreLevels, SemanticJudge, State,
};
use crate::state::{RuntimeState, StateKey};

/// Result of the latest OpenAI connection test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestRecord {
    /// Whether the probe got an answer that passed the judgement contract.
    pub ok: bool,
    /// When the probe finished, in Unix milliseconds.
    pub at_ms: u64,
    /// Probe round-trip time in milliseconds.
    pub latency_ms: u64,
    /// What happened, for the console; never contains the key.
    pub detail: String,
    /// Model the probe asked; a different configured model needs a new test.
    pub model: String,
}

/// Stored, non-secret judge settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredPrefs {
    provider: String,
    test: Option<TestRecord>,
}

/// In-memory judge selection; mirrors stored state once a change is saved.
#[derive(Debug, Clone, Default)]
struct Selection {
    key: Option<OpenAiApiKey>,
    openai: bool,
    test: Option<TestRecord>,
}

impl Selection {
    fn test_passed(&self) -> bool {
        self.test.as_ref().is_some_and(|test| test.ok)
    }
}

/// A refused judge change, rendered by [`routes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// HTTP status to answer with.
    pub status: StatusCode,
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Operator-facing explanation.
    pub reason: String,
}

impl Refusal {
    fn conflict(code: &'static str, reason: &str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
            reason: reason.to_owned(),
        }
    }

    fn not_saved(reason: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "judge_not_saved",
            reason: reason.into(),
        }
    }
}

/// Live judge selection plus the shared OpenAI Decisions client.
///
/// Cheap to clone; clones share one selection.
#[derive(Debug, Clone)]
pub struct JudgeControl {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    decisions: OpenAiDecisions,
    selection: Mutex<Selection>,
    /// Serializes changes, including the test's network round trip.
    changes: tokio::sync::Mutex<()>,
}

impl JudgeControl {
    /// Builds the control and its one shared OpenAI HTTP client. Nothing is
    /// selected until [`JudgeControl::restore`] or an operator change.
    ///
    /// # Errors
    /// Returns [`JevError::Transport`] when the HTTP client cannot be built.
    pub fn new(settings: &OpenAiSettings) -> Result<Self, JevError> {
        Ok(Self {
            inner: Arc::new(Inner {
                decisions: OpenAiDecisions::new(settings)?,
                selection: Mutex::new(Selection::default()),
                changes: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// OpenAI Decisions model the judge and its test use.
    pub fn model(&self) -> &str {
        self.inner.decisions.model()
    }

    /// The selected judge provider.
    pub fn provider(&self) -> JevProvider {
        if self.selection().openai {
            JevProvider::OpenAi
        } else {
            JevProvider::TypeSafe
        }
    }

    fn selection(&self) -> Selection {
        self.inner
            .selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut Selection)) {
        change(
            &mut self
                .inner
                .selection
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// Makes the configured judge runtime match the selection.
    fn apply(&self, jev: Option<&JevRuntime>) {
        let Some(jev) = jev else {
            return;
        };
        let selection = self.selection();
        match (selection.openai, selection.key) {
            (true, Some(key)) => jev.use_primary(Arc::new(self.inner.decisions.judge(key))),
            _ => jev.clear_primary(),
        }
    }

    /// Resumes the saved selection, key, and test result.
    ///
    /// A saved OpenAI selection resumes only while its key, a passing test
    /// of the configured model, and a TypeSafe fallback all still exist;
    /// otherwise TypeSafe answers and a warning says why.
    ///
    /// # Errors
    /// Returns a reason when stored state cannot be read, parsed, or
    /// decrypted, so startup fails instead of silently changing the judge.
    pub async fn restore(
        &self,
        runtime_state: &RuntimeState,
        vault: &CredentialVault,
        jev: Option<&JevRuntime>,
    ) -> Result<(), String> {
        let prefs = match runtime_state
            .load_required(StateKey::JudgePrefs)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(stored) => Some(
                serde_json::from_value::<StoredPrefs>(stored)
                    .map_err(|error| format!("stored judge settings are unreadable: {error}"))?,
            ),
            None => None,
        };
        let key = match runtime_state
            .load_required(StateKey::JudgeOpenAiKey)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(stored) => match vault.open_text(&stored)? {
                Some(plain) => Some(
                    OpenAiApiKey::parse(&plain)
                        .map_err(|_| "stored OpenAI key is malformed".to_owned())?,
                ),
                None => None,
            },
            None => None,
        };
        let Some(prefs) = prefs else {
            self.update(|selection| selection.key = key);
            return Ok(());
        };
        let provider = JevProvider::parse(&prefs.provider)
            .ok_or_else(|| format!("stored judge provider `{}` is unknown", prefs.provider))?;

        // A test proves one key against one model.
        let test = prefs.test.filter(|test| {
            let current = test.model == self.model();
            if !current {
                tracing::info!(
                    tested = test.model,
                    configured = self.model(),
                    "OpenAI judge model changed since its last test; a new test is needed"
                );
            }
            current
        });
        let mut selection = Selection {
            key,
            openai: false,
            test,
        };
        if provider == JevProvider::OpenAi {
            let blocker = if jev.is_none() {
                Some("no TypeSafe Jev is configured as its fallback")
            } else if selection.key.is_none() {
                Some("no OpenAI key is saved")
            } else if !selection.test_passed() {
                Some("its latest test did not pass for the configured model")
            } else {
                None
            };
            match blocker {
                Some(reason) => tracing::warn!(
                    reason,
                    "saved OpenAI judge selection cannot resume; TypeSafe Jev answers"
                ),
                None => selection.openai = true,
            }
        }
        let openai = selection.openai;
        self.update(|current| *current = selection);
        self.apply(jev);
        if openai {
            tracing::info!(
                model = self.model(),
                "resumed OpenAI Decisions as the primary judge with Jev fallback"
            );
        }
        Ok(())
    }

    /// Console view: selection, fallback availability, key hint, latest test,
    /// and how many judgements fell back.
    pub fn view(&self, state: &AppState) -> Value {
        let selection = self.selection();
        json!({
            "provider": self.provider().as_str(),
            "fallbackAvailable": state.jev().is_some(),
            "available": state.credential_vault().is_some() && state.runtime_state().enabled(),
            "openai": {
                "key": {
                    "set": selection.key.is_some(),
                    "hint": selection.key.as_ref().map(OpenAiApiKey::hint),
                },
                "model": self.model(),
                "test": selection.test,
                "fallbacks": state.jev().map_or(0, |jev| jev.usage().fallbacks),
            }
        })
    }

    /// Seals and saves a new key. The selection returns to TypeSafe and the
    /// old test is dropped first, so no stored state can pair the new key
    /// with an older passing test.
    ///
    /// # Errors
    /// Returns a [`Refusal`] when storage is unavailable or fails.
    pub async fn save_key(&self, state: &AppState, key: OpenAiApiKey) -> Result<(), Refusal> {
        let _change = self.inner.changes.lock().await;
        let Some(vault) = state.credential_vault() else {
            return Err(Refusal::not_saved("credential storage is unavailable"));
        };
        let sealed = vault.seal_text(key.expose()).map_err(Refusal::not_saved)?;
        self.withdraw(state).await?;
        state
            .runtime_state()
            .save_required(StateKey::JudgeOpenAiKey, &sealed)
            .await
            .map_err(|error| Refusal::not_saved(error.to_string()))?;
        self.update(|selection| selection.key = Some(key));
        Ok(())
    }

    /// Removes the saved key; TypeSafe answers from then on.
    ///
    /// # Errors
    /// Returns a [`Refusal`] when storage is unavailable or fails.
    pub async fn remove_key(&self, state: &AppState) -> Result<(), Refusal> {
        let _change = self.inner.changes.lock().await;
        self.withdraw(state).await?;
        state
            .runtime_state()
            .save_required(
                StateKey::JudgeOpenAiKey,
                &json!({"version": 1, "ciphertext": null}),
            )
            .await
            .map_err(|error| Refusal::not_saved(error.to_string()))?;
        self.update(|selection| selection.key = None);
        Ok(())
    }

    /// Saves and applies "TypeSafe, untested" ahead of a key change.
    async fn withdraw(&self, state: &AppState) -> Result<(), Refusal> {
        save_prefs(state, false, None).await?;
        self.update(|selection| {
            selection.openai = false;
            selection.test = None;
        });
        self.apply(state.jev());
        Ok(())
    }

    /// Sends one fixed probe (choice, noul, and score questions) through
    /// OpenAI only — never the fallback — and records the outcome. A failing
    /// test while OpenAI is selected returns the selection to TypeSafe.
    ///
    /// # Errors
    /// Returns a [`Refusal`] when no key is saved or the result cannot be
    /// saved. A probe that fails is a result, not an error.
    pub async fn test(&self, state: &AppState) -> Result<TestRecord, Refusal> {
        let _change = self.inner.changes.lock().await;
        let selection = self.selection();
        let Some(key) = selection.key.clone() else {
            return Err(Refusal::conflict(
                "openai_key_missing",
                "Save an OpenAI API key first.",
            ));
        };
        let judge = self.inner.decisions.judge(key);
        let started = Instant::now();
        let outcome = async { judge.judge(probe_request()?).await }.await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let record = TestRecord {
            ok: outcome.is_ok(),
            at_ms: unix_ms(state.now()),
            latency_ms,
            detail: match &outcome {
                Ok(response) => format!("Answered by {}", response.model()),
                Err(error) => describe(error),
            },
            model: self.model().to_owned(),
        };
        let keep_openai = selection.openai && record.ok;
        save_prefs(state, keep_openai, Some(&record)).await?;
        self.update(|current| {
            current.test = Some(record.clone());
            current.openai = keep_openai;
        });
        self.apply(state.jev());
        if selection.openai && !keep_openai {
            tracing::warn!(
                detail = record.detail,
                "OpenAI judge test failed; TypeSafe Jev answers until a test passes"
            );
        }
        Ok(record)
    }

    /// Selects the judge. OpenAI needs a TypeSafe fallback, a saved key, and
    /// a passing latest test; TypeSafe is always allowed.
    ///
    /// # Errors
    /// Returns a `409` [`Refusal`] naming the missing precondition, or a
    /// storage refusal.
    pub async fn select(&self, state: &AppState, provider: JevProvider) -> Result<(), Refusal> {
        let _change = self.inner.changes.lock().await;
        let selection = self.selection();
        let openai = provider == JevProvider::OpenAi;
        if openai {
            if state.jev().is_none() {
                return Err(Refusal::conflict(
                    "openai_needs_fallback",
                    "Configure TypeSafe Jev first; it answers whenever OpenAI cannot.",
                ));
            }
            if selection.key.is_none() {
                return Err(Refusal::conflict(
                    "openai_key_missing",
                    "Save an OpenAI API key first.",
                ));
            }
            if !selection.test_passed() {
                return Err(Refusal::conflict(
                    "openai_test_required",
                    "Run a passing test first.",
                ));
            }
        }
        save_prefs(state, openai, selection.test.as_ref()).await?;
        self.update(|current| current.openai = openai);
        self.apply(state.jev());
        tracing::info!(provider = provider.as_str(), "judge selection changed");
        Ok(())
    }
}

async fn save_prefs(
    state: &AppState,
    openai: bool,
    test: Option<&TestRecord>,
) -> Result<(), Refusal> {
    let provider = if openai {
        JevProvider::OpenAi
    } else {
        JevProvider::TypeSafe
    };
    state
        .runtime_state()
        .save_required(
            StateKey::JudgePrefs,
            &json!({ "provider": provider.as_str(), "test": test }),
        )
        .await
        .map_err(|error| Refusal::not_saved(error.to_string()))
}

/// The fixed connection probe: one question of each type over a neutral,
/// synthetic market description. Its answers are discarded.
fn probe_request() -> Result<JevRequest, JevError> {
    let mut questions = BTreeMap::new();
    questions.insert(
        "direction".to_owned(),
        Question::choice(
            Instructions::text("Which direction has the stronger evidence?")?,
            ChoiceOptions::new([
                (
                    "long".to_owned(),
                    Some("Evidence favours buying".to_owned()),
                ),
                (
                    "short".to_owned(),
                    Some("Evidence favours selling".to_owned()),
                ),
                ("flat".to_owned(), Some("No directional edge".to_owned())),
            ])?,
        ),
    );
    questions.insert(
        "trending".to_owned(),
        Question::noul(
            Instructions::text("Does this describe a trending market rather than a range?")?,
            NoulCriteria::default(),
        ),
    );
    questions.insert(
        "momentum".to_owned(),
        Question::score(
            Instructions::text("How strong is the directional momentum?")?,
            ScoreLevels::new(["Weak".to_owned(), "Neutral".to_owned(), "Strong".to_owned()])?,
        ),
    );
    JevRequest::new(
        State::text(
            "Connection test. EURUSD H4: the last five candles each closed higher, \
             with rising highs and lows.",
        )?,
        questions,
    )
}

/// Operator-facing wording for a failed probe.
fn describe(error: &JevError) -> String {
    match error {
        JevError::Denied { status, detail } => format!("{detail} ({status})"),
        JevError::Unauthorized => "Credential rejected".to_owned(),
        JevError::Rejected { detail } => format!("Request rejected: {detail}"),
        JevError::Unavailable { status } => format!("Rate limited or overloaded ({status})"),
        JevError::Transport { reason } => format!("Unreachable: {reason}"),
        JevError::MalformedResponse { reason } | JevError::Contract { reason } => {
            format!("Unexpected answer format: {reason}")
        }
    }
}

fn unix_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}
