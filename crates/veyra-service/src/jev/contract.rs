//! Typed contract for System One judgements: questions in, answers out.
//!
//! Everything a caller may put into a [`JevRequest`] and everything a transport
//! may hand back is parsed once here and then used only as validated data:
//! question ids, instructions, options, and levels on the way in;
//! probabilities, confidences, legends, and answer/option alignment on the way
//! out. Wire shapes mirror `POST /v1/systemone` (docs.typesafe.ai); the
//! transport owns HTTP, this module owns meaning.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use super::JevError;

/// Bounds that keep one judgement request reviewable and bounded.
const MAX_QUESTIONS: usize = 16;
const MAX_QUESTION_ID_CHARS: usize = 64;
const MAX_STATE_BYTES: usize = 64 * 1024;
const MAX_INSTRUCTIONS_BYTES: usize = 8 * 1024;
const MAX_LABEL_CHARS: usize = 512;
const MAX_OPTIONS: usize = 32;
const MAX_LEVELS: usize = 16;
const MIN_CHOICES: usize = 2;
const MIN_LEVELS: usize = 2;
/// Float slack allowed when checking that a distribution sums to one.
const PROBABILITY_TOLERANCE: f64 = 1e-3;

fn contract(reason: &str) -> JevError {
    JevError::Contract {
        reason: reason.to_owned(),
    }
}

/// Validated state to evaluate: text, object, or array.
#[derive(Debug, Clone, PartialEq)]
pub struct State(Value);

impl State {
    /// Wraps free text.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for blank or oversized text.
    pub fn text(value: &str) -> Result<Self, JevError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(contract("state text must not be blank"));
        }
        if trimmed.len() > MAX_STATE_BYTES {
            return Err(contract("state text is too large"));
        }
        Ok(Self(Value::String(trimmed.to_owned())))
    }

    /// Wraps structured state; null and bare scalars are rejected.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for unsupported shapes or oversized
    /// payloads.
    pub fn json(value: Value) -> Result<Self, JevError> {
        if !matches!(value, Value::Object(_) | Value::Array(_)) {
            return Err(contract("state must be text, an object, or an array"));
        }
        if value.to_string().len() > MAX_STATE_BYTES {
            return Err(contract("structured state is too large"));
        }
        Ok(Self(value))
    }

    /// Returns the value sent to the model.
    pub fn value(&self) -> &Value {
        &self.0
    }
}

/// Validated question instructions: text, object, or array.
#[derive(Debug, Clone, PartialEq)]
pub struct Instructions(Value);

impl Instructions {
    /// Wraps textual instructions.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for blank or oversized text.
    pub fn text(value: &str) -> Result<Self, JevError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(contract("instructions must not be blank"));
        }
        if trimmed.len() > MAX_INSTRUCTIONS_BYTES {
            return Err(contract("instructions are too large"));
        }
        Ok(Self(Value::String(trimmed.to_owned())))
    }

    /// Wraps structured instructions; null and bare scalars are rejected.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for unsupported shapes or oversized
    /// payloads.
    pub fn json(value: Value) -> Result<Self, JevError> {
        if !matches!(value, Value::Object(_) | Value::Array(_)) {
            return Err(contract(
                "instructions must be text, an object, or an array",
            ));
        }
        if value.to_string().len() > MAX_INSTRUCTIONS_BYTES {
            return Err(contract("structured instructions are too large"));
        }
        Ok(Self(value))
    }

    /// Returns the value sent to the model.
    pub fn value(&self) -> &Value {
        &self.0
    }
}

/// Optional yes/no clarifications for a noul question.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoulCriteria {
    yes: Option<String>,
    no: Option<String>,
}

impl NoulCriteria {
    /// Builds criteria from optional descriptions.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for blank or oversized descriptions.
    pub fn new(yes: Option<&str>, no: Option<&str>) -> Result<Self, JevError> {
        Ok(Self {
            yes: label(yes, "criteria.true")?,
            no: label(no, "criteria.false")?,
        })
    }

    /// Description of a yes answer, when provided.
    pub fn yes(&self) -> Option<&str> {
        self.yes.as_deref()
    }

    /// Description of a no answer, when provided.
    pub fn no(&self) -> Option<&str> {
        self.no.as_deref()
    }
}

/// Two to 32 named choice options, each with an optional rubric description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOptions(BTreeMap<String, Option<String>>);

impl ChoiceOptions {
    /// Builds the option map; option names must be unique.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for duplicate names, invalid labels, or
    /// a count outside 2-32.
    pub fn new(
        entries: impl IntoIterator<Item = (String, Option<String>)>,
    ) -> Result<Self, JevError> {
        let mut map = BTreeMap::new();
        for (option, description) in entries {
            let option = option.trim().to_owned();
            if option.is_empty() || option.len() > MAX_QUESTION_ID_CHARS {
                return Err(contract("choice options must be 1-64 characters"));
            }
            if option.chars().any(char::is_control) {
                return Err(contract(
                    "choice options must not contain control characters",
                ));
            }
            let description = label(description.as_deref(), "criteria")?;
            if map.insert(option, description).is_some() {
                return Err(contract("choice options must be unique"));
            }
        }
        if !(MIN_CHOICES..=MAX_OPTIONS).contains(&map.len()) {
            return Err(contract("a choice question needs 2-32 options"));
        }
        Ok(Self(map))
    }

    /// Returns the option map.
    pub fn options(&self) -> &BTreeMap<String, Option<String>> {
        &self.0
    }

    /// Whether `option` was offered.
    pub fn contains(&self, option: &str) -> bool {
        self.0.contains_key(option)
    }
}

/// Two to 16 ordered score levels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreLevels(Vec<String>);

impl ScoreLevels {
    /// Builds ordered levels from ordered descriptions.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for invalid labels or a count outside
    /// 2-16.
    pub fn new(levels: impl IntoIterator<Item = String>) -> Result<Self, JevError> {
        let mut collected = Vec::new();
        for level in levels {
            let level = label(Some(level.as_str()), "criteria")?
                .ok_or_else(|| contract("score levels must not be blank"))?;
            collected.push(level);
        }
        if !(MIN_LEVELS..=MAX_LEVELS).contains(&collected.len()) {
            return Err(contract("a score question needs 2-16 levels"));
        }
        Ok(Self(collected))
    }

    /// Returns the ordered levels.
    pub fn levels(&self) -> &[String] {
        &self.0
    }
}

/// One typed question for the model.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    /// Yes/no question; answers carry the probability of yes.
    Noul {
        /// The question to evaluate.
        instructions: Instructions,
        /// Optional descriptions of what yes and no mean.
        criteria: NoulCriteria,
    },
    /// Pick one of a defined set of options.
    Choice {
        /// What the model should decide.
        instructions: Instructions,
        /// The offered options.
        options: ChoiceOptions,
    },
    /// Rate the state along ordered levels.
    Score {
        /// What the model should rate.
        instructions: Instructions,
        /// The ordered levels.
        levels: ScoreLevels,
    },
}

impl Question {
    /// Builds a noul question from validated parts.
    pub fn noul(instructions: Instructions, criteria: NoulCriteria) -> Self {
        Self::Noul {
            instructions,
            criteria,
        }
    }

    /// Builds a choice question from validated parts.
    pub fn choice(instructions: Instructions, options: ChoiceOptions) -> Self {
        Self::Choice {
            instructions,
            options,
        }
    }

    /// Builds a score question from validated parts.
    pub fn score(instructions: Instructions, levels: ScoreLevels) -> Self {
        Self::Score {
            instructions,
            levels,
        }
    }

    pub(crate) fn wire(&self) -> Value {
        let mut question = serde_json::Map::new();
        match self {
            Self::Noul {
                instructions,
                criteria,
            } => {
                question.insert("type".to_owned(), Value::String("noul".to_owned()));
                question.insert("instructions".to_owned(), instructions.value().clone());
                let mut criteria_map = serde_json::Map::new();
                if let Some(yes) = criteria.yes() {
                    criteria_map.insert("true".to_owned(), Value::String(yes.to_owned()));
                }
                if let Some(no) = criteria.no() {
                    criteria_map.insert("false".to_owned(), Value::String(no.to_owned()));
                }
                if !criteria_map.is_empty() {
                    question.insert("criteria".to_owned(), Value::Object(criteria_map));
                }
            }
            Self::Choice {
                instructions,
                options,
            } => {
                question.insert("type".to_owned(), Value::String("choice".to_owned()));
                question.insert("instructions".to_owned(), instructions.value().clone());
                let criteria = options
                    .options()
                    .iter()
                    .map(|(option, description)| {
                        (
                            option.clone(),
                            description
                                .clone()
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        )
                    })
                    .collect::<serde_json::Map<String, Value>>();
                question.insert("criteria".to_owned(), Value::Object(criteria));
            }
            Self::Score {
                instructions,
                levels,
            } => {
                question.insert("type".to_owned(), Value::String("score".to_owned()));
                question.insert("instructions".to_owned(), instructions.value().clone());
                let criteria = levels
                    .levels()
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<Value>>();
                question.insert("criteria".to_owned(), Value::Array(criteria));
            }
        }
        Value::Object(question)
    }
}

impl Serialize for Question {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.wire().serialize(serializer)
    }
}

/// Validated probability in the closed unit interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probability(f64);

impl Probability {
    fn parse(field: &str, value: f64) -> Result<Self, JevError> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(contract(&format!("{field} must be between 0 and 1")))
        }
    }

    /// Returns the accepted probability.
    pub fn value(self) -> f64 {
        self.0
    }
}

/// Validated answer confidence in the closed unit interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Confidence(f64);

impl Confidence {
    fn parse(value: f64) -> Result<Self, JevError> {
        Probability::parse("confidence", value).map(|probability| Self(probability.value()))
    }

    /// Returns the accepted confidence.
    pub fn value(self) -> f64 {
        self.0
    }
}

/// Answer to a noul question.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoulAnswer {
    probability: Probability,
}

impl NoulAnswer {
    /// Returns the probability that the answer is yes.
    pub fn probability(&self) -> Probability {
        self.probability
    }
}

/// Answer to a choice question.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    choice: String,
    probabilities: BTreeMap<String, Probability>,
    confidence: Confidence,
}

impl ChoiceAnswer {
    /// Returns the highest-probability option.
    pub fn choice(&self) -> &str {
        &self.choice
    }

    /// Returns every offered option with its probability.
    pub fn probabilities(&self) -> &BTreeMap<String, Probability> {
        &self.probabilities
    }

    /// Returns the answer confidence.
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }
}

/// Answer to a score question.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreAnswer {
    score: f64,
    legend: BTreeMap<u32, String>,
    probabilities: BTreeMap<u32, Probability>,
    confidence: Confidence,
}

impl ScoreAnswer {
    /// Returns the probability-weighted value across levels.
    pub fn score(&self) -> f64 {
        self.score
    }

    /// Returns each level number mapped back to its description.
    pub fn legend(&self) -> &BTreeMap<u32, String> {
        &self.legend
    }

    /// Returns each level with its probability.
    pub fn probabilities(&self) -> &BTreeMap<u32, Probability> {
        &self.probabilities
    }

    /// Returns the answer confidence.
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }
}

/// One typed answer, matched to the question that produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// Noul answer.
    Noul(NoulAnswer),
    /// Choice answer.
    Choice(ChoiceAnswer),
    /// Score answer.
    Score(ScoreAnswer),
}

/// Token usage reported by the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
}

/// Validated judgement request.
#[derive(Debug, Clone, PartialEq)]
pub struct JevRequest {
    state: State,
    questions: BTreeMap<String, Question>,
}

impl JevRequest {
    /// Builds a request from validated state and questions.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for an empty or oversized question set,
    /// or for question ids that are not stable code identifiers.
    pub fn new(state: State, questions: BTreeMap<String, Question>) -> Result<Self, JevError> {
        if questions.is_empty() || questions.len() > MAX_QUESTIONS {
            return Err(contract("a request needs 1-16 questions"));
        }
        for id in questions.keys() {
            let valid = !id.is_empty()
                && id.len() <= MAX_QUESTION_ID_CHARS
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            if !valid {
                return Err(contract(
                    "question ids must be 1-64 characters of letters, digits, '_', '-' or '.'",
                ));
            }
        }
        Ok(Self { state, questions })
    }

    /// Returns the state to evaluate.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Returns the typed questions keyed by id.
    pub fn questions(&self) -> &BTreeMap<String, Question> {
        &self.questions
    }

    /// Verifies that a response answers exactly this request's questions with
    /// the same types and the same option or level vocabulary.
    ///
    /// # Errors
    /// Returns [`JevError::Contract`] for missing or unexpected answers,
    /// mismatched answer types, unoffered choice options, or a score legend
    /// that does not match the requested levels.
    pub fn validate_answers(&self, response: &JevResponse) -> Result<(), JevError> {
        if response.answers().len() != self.questions.len() {
            return Err(contract("answer count does not match question count"));
        }
        for (id, question) in &self.questions {
            let answer = response
                .answers()
                .get(id)
                .ok_or_else(|| contract("response is missing an answer for a question"))?;
            match (question, answer) {
                (Question::Noul { .. }, Answer::Noul(_)) => {}
                (Question::Choice { options, .. }, Answer::Choice(choice)) => {
                    if !options.contains(choice.choice()) {
                        return Err(contract("the chosen option was not offered"));
                    }
                }
                (Question::Score { levels, .. }, Answer::Score(score)) => {
                    if score.legend().len() != levels.levels().len() {
                        return Err(contract("score legend does not match the requested levels"));
                    }
                    for (index, level) in levels.levels().iter().enumerate() {
                        let expected =
                            u32::try_from(index).map_err(|_| contract("too many score levels"))?;
                        match score.legend().get(&expected) {
                            Some(label) if label == level => {}
                            _ => {
                                return Err(contract(
                                    "score legend does not match the requested levels",
                                ));
                            }
                        }
                    }
                }
                _ => return Err(contract("answer type does not match question type")),
            }
        }
        Ok(())
    }
}

/// Validated judgement response.
#[derive(Debug, Clone, PartialEq)]
pub struct JevResponse {
    model: String,
    answers: BTreeMap<String, Answer>,
    usage: Usage,
}

impl JevResponse {
    /// Returns the concrete model that answered (for example `jev-1.13.0`).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns every answer keyed by question id.
    pub fn answers(&self) -> &BTreeMap<String, Answer> {
        &self.answers
    }

    /// Returns one answer by question id.
    pub fn answer(&self, id: &str) -> Option<&Answer> {
        self.answers.get(id)
    }

    /// Returns token usage.
    pub fn usage(&self) -> Usage {
        self.usage
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    model: String,
    answers: BTreeMap<String, WireAnswer>,
    usage: WireUsage,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
}

fn total_probability(values: impl Iterator<Item = f64>) -> f64 {
    values.sum()
}

impl TryFrom<WireResponse> for JevResponse {
    type Error = JevError;

    fn try_from(wire: WireResponse) -> Result<Self, Self::Error> {
        if wire.model.trim().is_empty() || wire.model.len() > 128 {
            return Err(contract("model name is missing or oversized"));
        }
        if wire.model.chars().any(char::is_control) {
            return Err(contract("model name contains control characters"));
        }
        if wire.answers.is_empty() {
            return Err(contract("response contains no answers"));
        }

        let mut answers = BTreeMap::new();
        for (id, answer) in wire.answers {
            let parsed = match answer {
                WireAnswer::Noul { noul } => Answer::Noul(NoulAnswer {
                    probability: Probability::parse("noul", noul)?,
                }),
                WireAnswer::Choice {
                    choice,
                    probabilities,
                    confidence,
                } => {
                    if probabilities.len() < MIN_CHOICES {
                        return Err(contract("choice answers need at least two probabilities"));
                    }
                    let parsed = probabilities
                        .into_iter()
                        .map(|(option, probability)| {
                            Ok((option, Probability::parse("probabilities", probability)?))
                        })
                        .collect::<Result<BTreeMap<_, _>, JevError>>()?;
                    let sum = total_probability(parsed.values().map(|value| value.value()));
                    if (sum - 1.0).abs() > PROBABILITY_TOLERANCE {
                        return Err(contract("choice probabilities must sum to 1"));
                    }
                    let chosen = parsed
                        .get(&choice)
                        .ok_or_else(|| contract("the chosen option has no probability"))?;
                    if parsed
                        .values()
                        .any(|value| value.value() > chosen.value() + PROBABILITY_TOLERANCE)
                    {
                        return Err(contract(
                            "the chosen option is not the highest-probability option",
                        ));
                    }
                    Answer::Choice(ChoiceAnswer {
                        choice,
                        probabilities: parsed,
                        confidence: Confidence::parse(confidence)?,
                    })
                }
                WireAnswer::Score {
                    score,
                    legend,
                    probabilities,
                    confidence,
                } => {
                    if legend.len() < MIN_LEVELS || probabilities.len() != legend.len() {
                        return Err(contract(
                            "score answers need matching legend and probability levels",
                        ));
                    }
                    let legend = legend
                        .into_iter()
                        .map(|(level, label)| {
                            let index = level
                                .parse::<u32>()
                                .map_err(|_| contract("score legend keys must be level numbers"))?;
                            Ok((index, label))
                        })
                        .collect::<Result<BTreeMap<_, _>, JevError>>()?;
                    let probabilities = probabilities
                        .into_iter()
                        .map(|(level, probability)| {
                            let index = level.parse::<u32>().map_err(|_| {
                                contract("score probability keys must be level numbers")
                            })?;
                            Ok((index, Probability::parse("probabilities", probability)?))
                        })
                        .collect::<Result<BTreeMap<_, _>, JevError>>()?;
                    if legend.keys().ne(probabilities.keys()) {
                        return Err(contract("score legend and probabilities disagree"));
                    }
                    let sum = total_probability(probabilities.values().map(|value| value.value()));
                    if (sum - 1.0).abs() > PROBABILITY_TOLERANCE {
                        return Err(contract("score probabilities must sum to 1"));
                    }
                    let highest = (legend.len() - 1) as f64;
                    if !score.is_finite()
                        || score < -PROBABILITY_TOLERANCE
                        || score > highest + PROBABILITY_TOLERANCE
                    {
                        return Err(contract("score is outside the reported levels"));
                    }
                    Answer::Score(ScoreAnswer {
                        score,
                        legend,
                        probabilities,
                        confidence: Confidence::parse(confidence)?,
                    })
                }
            };
            answers.insert(id, parsed);
        }

        Ok(Self {
            model: wire.model.trim().to_owned(),
            answers,
            usage: Usage {
                input_tokens: wire.usage.input_tokens,
                output_tokens: wire.usage.output_tokens,
            },
        })
    }
}

/// Parses and validates one `POST /v1/systemone` response body.
pub(crate) fn parse_response_body(bytes: &[u8]) -> Result<JevResponse, JevError> {
    let wire: WireResponse =
        serde_json::from_slice(bytes).map_err(|error| JevError::MalformedResponse {
            reason: format!("response is not the documented JSON: {error}"),
        })?;
    JevResponse::try_from(wire)
}

fn label(value: Option<&str>, field: &str) -> Result<Option<String>, JevError> {
    match value {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.len() > MAX_LABEL_CHARS {
                return Err(contract(&format!("{field} must be 1-512 characters")));
            }
            if trimmed.chars().any(char::is_control) {
                return Err(contract(&format!(
                    "{field} must not contain control characters"
                )));
            }
            Ok(Some(trimmed.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn instructions() -> Instructions {
        Instructions::text("Does this state look risky?").expect("valid text")
    }

    fn choice_question() -> Question {
        Question::choice(
            instructions(),
            ChoiceOptions::new([
                ("long".to_owned(), Some("Favours buying".to_owned())),
                ("short".to_owned(), None),
                ("flat".to_owned(), None),
            ])
            .expect("valid options"),
        )
    }

    fn score_question() -> Question {
        Question::score(
            instructions(),
            ScoreLevels::new(["Weak".to_owned(), "Neutral".to_owned(), "Strong".to_owned()])
                .expect("valid levels"),
        )
    }

    fn request() -> JevRequest {
        let mut questions = BTreeMap::new();
        questions.insert("direction".to_owned(), choice_question());
        questions.insert(
            "is_trending".to_owned(),
            Question::noul(
                instructions(),
                NoulCriteria::new(Some("Trending"), Some("Range-bound")).expect("criteria"),
            ),
        );
        questions.insert("momentum".to_owned(), score_question());
        JevRequest::new(
            State::text("EURUSD closed above its average.").expect("state"),
            questions,
        )
        .expect("request")
    }

    #[test]
    fn questions_serialize_to_the_documented_wire_shape() {
        let value = serde_json::to_value(choice_question()).expect("serializable");
        assert_eq!(
            value,
            json!({
                "type": "choice",
                "instructions": "Does this state look risky?",
                "criteria": {"long": "Favours buying", "short": null, "flat": null}
            })
        );

        let noul = serde_json::to_value(Question::noul(instructions(), NoulCriteria::default()))
            .expect("serializable");
        assert_eq!(noul["type"], "noul");
        assert!(noul.get("criteria").is_none(), "empty criteria is omitted");

        let score = serde_json::to_value(score_question()).expect("serializable");
        assert_eq!(score["type"], "score");
        assert_eq!(score["criteria"], json!(["Weak", "Neutral", "Strong"]));
    }

    #[test]
    fn request_ids_are_stable_code_identifiers() {
        let mut questions = BTreeMap::new();
        questions.insert("bad id".to_owned(), choice_question());
        let error = JevRequest::new(State::text("state").expect("state"), questions)
            .expect_err("space is not allowed");
        assert!(matches!(error, JevError::Contract { .. }));

        let error = JevRequest::new(State::text("state").expect("state"), BTreeMap::new())
            .expect_err("empty questions must fail");
        assert!(matches!(error, JevError::Contract { .. }));
    }

    #[test]
    fn inputs_reject_unsupported_shapes_and_sizes() {
        assert!(State::text("  ").is_err());
        assert!(State::json(json!(42)).is_err());
        assert!(State::json(json!({"ok": true})).is_ok());
        assert!(Instructions::json(json!(true)).is_err());
        assert!(Instructions::text(" ").is_err());

        let long = "x".repeat(MAX_LABEL_CHARS + 1);
        assert!(NoulCriteria::new(Some(&long), None).is_err());
        assert!(NoulCriteria::new(Some("ok"), Some("also fine")).is_ok());

        let too_few = ChoiceOptions::new([("only".to_owned(), None)]);
        assert!(too_few.is_err());
        let duplicate = ChoiceOptions::new([("a".to_owned(), None), ("a".to_owned(), None)]);
        assert!(duplicate.is_err());
        let blank = ChoiceOptions::new([("".to_owned(), None), ("b".to_owned(), None)]);
        assert!(blank.is_err());

        assert!(ScoreLevels::new(["only".to_owned()]).is_err());
        assert!(ScoreLevels::new(["a".to_owned(), " ".to_owned()]).is_err());
    }

    #[test]
    fn documented_responses_parse_into_typed_answers() {
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {
                "direction": {
                    "type": "choice",
                    "choice": "long",
                    "probabilities": {"long": 0.99, "flat": 0.0, "short": 0.01},
                    "confidence": 0.98
                },
                "is_trending": {"type": "noul", "noul": 0.78},
                "momentum": {
                    "type": "score",
                    "score": 1.96,
                    "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"},
                    "probabilities": {"0": 0.01, "1": 0.03, "2": 0.96},
                    "confidence": 0.94
                }
            },
            "usage": {"input_tokens": 402, "output_tokens": 73}
        })
        .to_string();

        let response = parse_response_body(body.as_bytes()).expect("valid response");
        assert_eq!(response.model(), "jev-1.13.0");
        assert_eq!(response.usage().input_tokens, 402);

        match response.answer("direction").expect("answer") {
            Answer::Choice(choice) => {
                assert_eq!(choice.choice(), "long");
                assert_eq!(choice.confidence().value(), 0.98);
                assert_eq!(
                    choice
                        .probabilities()
                        .get("short")
                        .map(|value| value.value()),
                    Some(0.01)
                );
            }
            other => panic!("unexpected answer {other:?}"),
        }
        match response.answer("is_trending").expect("answer") {
            Answer::Noul(noul) => assert_eq!(noul.probability().value(), 0.78),
            other => panic!("unexpected answer {other:?}"),
        }
        match response.answer("momentum").expect("answer") {
            Answer::Score(score) => {
                assert_eq!(score.score(), 1.96);
                assert_eq!(score.legend().get(&2).map(String::as_str), Some("Strong"));
            }
            other => panic!("unexpected answer {other:?}"),
        }

        request()
            .validate_answers(&response)
            .expect("answers are aligned with the questions");
    }

    #[test]
    fn malformed_answers_are_rejected() {
        let cases = [
            json!({"model": "m", "answers": {}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "", "answers": {"q": {"type": "noul", "noul": 0.5}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "noul", "noul": 1.5}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "noul", "noul": "yes"}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "choice", "choice": "long", "probabilities": {"long": 0.9}, "confidence": 0.9}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "choice", "choice": "short", "probabilities": {"long": 0.9, "short": 0.1}, "confidence": 0.9}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "choice", "choice": "long", "probabilities": {"long": 0.9, "short": 0.4}, "confidence": 0.9}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "choice", "choice": "long", "probabilities": {"long": 0.9, "short": 0.1}, "confidence": 2.0}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "score", "score": 1.0, "legend": {"0": "a"}, "probabilities": {"0": 1.0}, "confidence": 0.5}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "score", "score": 1.0, "legend": {"zero": "a", "one": "b"}, "probabilities": {"0": 0.5, "1": 0.5}, "confidence": 0.5}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "score", "score": 3.0, "legend": {"0": "a", "1": "b"}, "probabilities": {"0": 0.5, "1": 0.5}, "confidence": 0.5}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            json!({"model": "m", "answers": {"q": {"type": "score", "score": 0.5, "legend": {"0": "a", "1": "b", "2": "c"}, "probabilities": {"0": 0.5, "1": 0.5}, "confidence": 0.5}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
        ];
        for case in cases {
            let body = case.to_string();
            assert!(
                parse_response_body(body.as_bytes()).is_err(),
                "expected rejection for {body}"
            );
        }

        assert!(parse_response_body(b"not json").is_err());
    }

    #[test]
    fn answer_alignment_is_checked_against_the_request() {
        let response = parse_response_body(
            json!({
                "model": "m",
                "answers": {
                    "direction": {"type": "choice", "choice": "sideways", "probabilities": {"sideways": 0.6, "long": 0.4}, "confidence": 0.5},
                    "is_trending": {"type": "noul", "noul": 0.5},
                    "momentum": {"type": "score", "score": 1.0, "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"}, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}, "confidence": 0.5}
                },
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })
            .to_string()
            .as_bytes(),
        )
        .expect("structurally valid");
        let error = request()
            .validate_answers(&response)
            .expect_err("unoffered option must fail");
        assert!(matches!(error, JevError::Contract { .. }));

        let wrong_type = parse_response_body(
            json!({
                "model": "m",
                "answers": {
                    "direction": {"type": "noul", "noul": 0.5},
                    "is_trending": {"type": "noul", "noul": 0.5},
                    "momentum": {"type": "score", "score": 1.0, "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"}, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}, "confidence": 0.5}
                },
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })
            .to_string()
            .as_bytes(),
        )
        .expect("structurally valid");
        assert!(request().validate_answers(&wrong_type).is_err());

        let missing = parse_response_body(
            json!({
                "model": "m",
                "answers": {
                    "is_trending": {"type": "noul", "noul": 0.5},
                    "momentum": {"type": "score", "score": 1.0, "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"}, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}, "confidence": 0.5}
                },
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })
            .to_string()
            .as_bytes(),
        )
        .expect("structurally valid");
        assert!(request().validate_answers(&missing).is_err());
    }
}
