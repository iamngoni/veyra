//! Output bounds for assistant tool results.
//!
//! The model layer discards any tool result whose serialized form exceeds its
//! own ceiling (12,000 characters) and substitutes `tool_result_too_large`,
//! which leaves the model with nothing. Tools here therefore fit their own
//! output below [`MAX_OUTPUT_CHARS`]: free text is clipped on character
//! boundaries, and lists keep their leading (newest or most relevant) rows
//! and say how many were omitted, so an answer is always possible and a
//! partial list is never mistaken for a complete one.

use serde_json::{Map, Value, json};

/// Serialized ceiling for one tool result, below the model layer's 12,000.
pub(super) const MAX_OUTPUT_CHARS: usize = 11_000;

/// Preferred bound for one recorded text field (reason, rationale, error).
pub(super) const MAX_TEXT_CHARS: usize = 1_200;

/// Looser text bounds tried, in order, before the tightest one.
const LOOSER_TEXT_TIERS: [usize; 2] = [MAX_TEXT_CHARS, 500];

/// Tightest text bound; below it rows are dropped instead.
const TIGHTEST_TEXT_CHARS: usize = 240;

/// Clips `text` to `max` characters, marking a cut with `…`.
pub(super) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

/// Serialized length of `value` in characters.
pub(super) fn serialized_chars(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.chars().count())
        .unwrap_or(usize::MAX)
}

/// Places the longest prefix of `items` under `field` that keeps `envelope`
/// within [`MAX_OUTPUT_CHARS`]. When rows are dropped, `<field>_omitted`
/// reports how many, so the model knows the list is partial.
pub(super) fn fit_list(envelope: Map<String, Value>, field: &str, items: Vec<Value>) -> Value {
    let build = |keep: usize| {
        let mut object = envelope.clone();
        object.insert(field.to_owned(), Value::Array(items[..keep].to_vec()));
        if keep < items.len() {
            object.insert(format!("{field}_omitted"), json!(items.len() - keep));
        }
        Value::Object(object)
    };
    let fits = |keep: usize| serialized_chars(&build(keep)) <= MAX_OUTPUT_CHARS;
    if fits(items.len()) {
        return build(items.len());
    }
    // Monotonic: fewer rows never serialize longer. Find the largest fit.
    let (mut low, mut high) = (0, items.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    build(low)
}

/// Like [`fit_list`], but first tightens the per-field text bound (1,200,
/// then 500, then 240 characters) so every requested row survives when
/// possible; rows are dropped only at the tightest bound. `rows` builds the
/// list for one bound, and the bound used is reported as `text_limit_chars`.
pub(super) fn fit_text_rows(
    envelope: Map<String, Value>,
    field: &str,
    rows: impl Fn(usize) -> Vec<Value>,
) -> Value {
    let attempt = |bound: usize| {
        let mut object = envelope.clone();
        object.insert("text_limit_chars".to_owned(), json!(bound));
        fit_list(object, field, rows(bound))
    };
    for bound in LOOSER_TEXT_TIERS {
        let fitted = attempt(bound);
        if fitted.get(format!("{field}_omitted")).is_none() {
            return fitted;
        }
    }
    attempt(TIGHTEST_TEXT_CHARS)
}

/// Inserts `value` under `key` unless it is JSON `null`, keeping rows compact.
pub(super) fn put(object: &mut Map<String, Value>, key: &str, value: Value) {
    if !value.is_null() {
        object.insert(key.to_owned(), value);
    }
}

/// Rounds money to cents for stable, readable output.
pub(super) fn cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_respects_character_boundaries() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("ééééé", 3), "ééé…");
        assert_eq!(clip("", 0), "");
        assert_eq!(clip("abc", 0), "…");
    }

    #[test]
    fn lists_fit_the_output_ceiling_and_report_omissions() {
        let small = fit_list(Map::new(), "rows", vec![json!(1), json!(2)]);
        assert_eq!(small["rows"].as_array().map(Vec::len), Some(2));
        assert!(small.get("rows_omitted").is_none());

        let rows: Vec<Value> = (0..400)
            .map(|index| json!({"index": index, "text": "x".repeat(100)}))
            .collect();
        let fitted = fit_list(Map::new(), "rows", rows);
        let kept = fitted["rows"].as_array().expect("rows").len();
        assert!(kept > 0 && kept < 400);
        assert_eq!(fitted["rows_omitted"], json!(400 - kept));
        assert!(serialized_chars(&fitted) <= MAX_OUTPUT_CHARS);
        assert_eq!(fitted["rows"][0]["index"], 0, "leading rows survive");

        let giant = fit_list(
            Map::new(),
            "rows",
            vec![json!("y".repeat(MAX_OUTPUT_CHARS))],
        );
        assert_eq!(giant["rows"].as_array().map(Vec::len), Some(0));
        assert_eq!(giant["rows_omitted"], 1);
    }

    #[test]
    fn text_rows_tighten_before_dropping() {
        let rows = |count: usize| {
            move |bound: usize| {
                (0..count)
                    .map(|index| json!({"index": index, "rationale": clip(&"r".repeat(2_000), bound)}))
                    .collect::<Vec<_>>()
            }
        };
        let roomy = fit_text_rows(Map::new(), "events", rows(3));
        assert_eq!(roomy["text_limit_chars"], 1_200);
        assert!(roomy.get("events_omitted").is_none());

        let tighter = fit_text_rows(Map::new(), "events", rows(15));
        assert_eq!(tighter["text_limit_chars"], 500);
        assert_eq!(tighter["events"].as_array().map(Vec::len), Some(15));

        let crowded = fit_text_rows(Map::new(), "events", rows(80));
        assert_eq!(crowded["text_limit_chars"], 240);
        assert!(
            crowded["events_omitted"]
                .as_u64()
                .is_some_and(|omitted| omitted > 0)
        );
        assert!(serialized_chars(&crowded) <= MAX_OUTPUT_CHARS);
    }

    #[test]
    fn helpers_keep_rows_compact() {
        let mut object = Map::new();
        put(&mut object, "kept", json!(1));
        put(&mut object, "dropped", Value::Null);
        assert_eq!(Value::Object(object), json!({"kept": 1}));
        assert_eq!(cents(1.234_9), 1.23);
        assert_eq!(cents(-0.005_1), -0.01);
    }
}
