//! Live TypeSafe/Jev proof. Ignored by default.
//!
//! Requires `VEYRA_JEV_API_KEY` (source `.env`) and network access.
//! Run with: `cargo test --test jev_live -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use veyra_service::jev::{
    ChoiceOptions, Instructions, JevRequest, JevRuntime, JevSettings, Question, ScoreLevels, State,
};

#[actix_web::test]
#[ignore = "requires VEYRA_JEV_API_KEY and network access"]
async fn jev_answers_typed_questions() {
    let settings = JevSettings::from_env()
        .expect("jev settings must parse")
        .expect("VEYRA_JEV_API_KEY must be configured");
    let runtime = JevRuntime::from_settings(settings).expect("jev runtime builds");
    assert_eq!(runtime.provider().as_str(), "typesafe");

    let mut questions = BTreeMap::new();
    questions.insert(
        "direction".to_owned(),
        Question::choice(
            Instructions::text("Which direction has the strongest evidence?").expect("valid"),
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
            ])
            .expect("valid options"),
        ),
    );
    questions.insert(
        "is_trending".to_owned(),
        Question::noul(
            Instructions::text("Does this describe a trending market?").expect("valid"),
            Default::default(),
        ),
    );
    questions.insert(
        "momentum".to_owned(),
        Question::score(
            Instructions::text("How strong is the directional momentum?").expect("valid"),
            ScoreLevels::new(["Weak".to_owned(), "Neutral".to_owned(), "Strong".to_owned()])
                .expect("valid levels"),
        ),
    );

    let request = JevRequest::new(
        State::text(
            "EURUSD H4 closed above its 20-period average after three consecutive up days. \
             The latest US CPI print came in cooler than expected.",
        )
        .expect("valid state"),
        questions,
    )
    .expect("valid request");

    let started = Instant::now();
    let response = runtime
        .judge()
        .judge(request)
        .await
        .expect("live judgement must succeed");
    let elapsed = started.elapsed();

    println!(
        "jev model={} latency={:?} usage={:?}",
        response.model(),
        elapsed,
        response.usage()
    );
    for (id, answer) in response.answers() {
        println!("  {id}: {answer:?}");
    }

    assert!(response.answers().len() == 3);
    assert!(elapsed < Duration::from_secs(20));
}
