//! Live model-provider proof. Ignored by default.
//!
//! Requires `VEYRA_MODEL_*` settings (source `.env`) and network access to
//! OpenRouter. Run with:
//! `cargo test --test model_live -- --ignored --nocapture`

use serde_json::json;

use veyra_service::model::{AnswerFormat, DecisionRequest, ModelRuntime, ModelSettings, ModelTier};

#[actix_web::test]
#[ignore = "requires VEYRA_MODEL_* settings and network access"]
async fn openrouter_answers_with_the_configured_schema() {
    let settings = ModelSettings::from_env()
        .expect("model settings must parse")
        .expect("VEYRA_MODEL_API_KEY must be configured");
    let runtime = ModelRuntime::from_settings(settings).expect("model runtime builds");
    assert_eq!(runtime.provider().as_str(), "openrouter");

    let answer = runtime
        .engine()
        .answer(DecisionRequest {
            instructions:
                "You classify short-term market bias. Answer only with the schema fields."
                    .to_owned(),
            input: "EURUSD closed above its 20-day average after three consecutive up days."
                .to_owned(),
            format: AnswerFormat {
                name: "bias".to_owned(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "bias": {"type": "string", "enum": ["bullish", "bearish", "neutral"]},
                        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                        "reasoning": {"type": "string"}
                    },
                    "required": ["bias", "confidence", "reasoning"]
                }),
            },
            tier: ModelTier::Fast,
        })
        .await
        .expect("model must answer");

    let bias = answer.value["bias"]
        .as_str()
        .expect("bias must be a string");
    assert!(matches!(bias, "bullish" | "bearish" | "neutral"));
    let confidence = answer.value["confidence"]
        .as_f64()
        .expect("confidence must be numeric");
    assert!((0.0..=1.0).contains(&confidence));
    println!("model answered: {}", answer.value);
}
