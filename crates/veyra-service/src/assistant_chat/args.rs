//! Validation for model-supplied tool arguments.
//!
//! Tool arguments are untrusted model output. Every tool names the keys it
//! accepts; anything else is refused with a bounded reason the model can act
//! on, and every value is range-checked before it reaches storage or the
//! broker. A JSON `null` means "not provided" (strict tool schemas often send
//! optional fields as null); any other wrong type is an error, never a default.

use serde_json::{Map, Value};

use super::clock::{Edge, OperatorOffset, resolve_instant};
use crate::broker::Symbol;

/// Longest free-text argument (keywords, outcomes, RFC 3339 instants).
const MAX_TEXT_ARGUMENT_CHARS: usize = 64;

/// One tool call's validated argument object.
#[derive(Debug)]
pub(super) struct Args<'a> {
    object: &'a Map<String, Value>,
}

impl<'a> Args<'a> {
    /// Accepts an argument object whose keys are all in `allowed`.
    ///
    /// # Errors
    /// Returns a bounded reason for a non-object or an unsupported key.
    pub(super) fn new(
        tool: &'static str,
        arguments: &'a Value,
        allowed: &[&str],
    ) -> Result<Self, String> {
        let object = arguments
            .as_object()
            .ok_or_else(|| "tool arguments must be an object".to_owned())?;
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            let key: String = key.chars().take(MAX_TEXT_ARGUMENT_CHARS).collect();
            return Err(if allowed.is_empty() {
                format!("{tool} does not accept arguments (got `{key}`)")
            } else {
                format!(
                    "unsupported {tool} argument `{key}`; accepted: {}",
                    allowed.join(", ")
                )
            });
        }
        Ok(Self { object })
    }

    fn get(&self, name: &str) -> Option<&'a Value> {
        self.object.get(name).filter(|value| !value.is_null())
    }

    /// Optional integer within `min..=max`.
    ///
    /// # Errors
    /// Returns a bounded reason for a non-integer or out-of-range value.
    pub(super) fn integer(&self, name: &str, min: i64, max: i64) -> Result<Option<i64>, String> {
        self.get(name)
            .map(|value| {
                value
                    .as_i64()
                    .filter(|number| (min..=max).contains(number))
                    .ok_or_else(|| format!("{name} must be an integer from {min} through {max}"))
            })
            .transpose()
    }

    /// Optional `days` window, 1 through 365.
    ///
    /// # Errors
    /// Returns a bounded reason outside that range.
    pub(super) fn days(&self) -> Result<Option<u32>, String> {
        Ok(self
            .integer("days", 1, 365)?
            .and_then(|days| u32::try_from(days).ok()))
    }

    /// Optional positive venue ticket.
    ///
    /// # Errors
    /// Returns a bounded reason for a non-positive or non-integer value.
    pub(super) fn ticket(&self) -> Result<Option<i64>, String> {
        self.integer("ticket", 1, i64::MAX)
    }

    /// Optional boolean flag.
    ///
    /// # Errors
    /// Returns a bounded reason for a non-boolean value.
    pub(super) fn flag(&self, name: &str) -> Result<Option<bool>, String> {
        self.get(name)
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| format!("{name} must be true or false"))
            })
            .transpose()
    }

    /// Optional short text.
    ///
    /// # Errors
    /// Returns a bounded reason for a non-string, empty, or long value.
    pub(super) fn text(&self, name: &str) -> Result<Option<&'a str>, String> {
        self.get(name)
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|text| {
                        !text.is_empty() && text.chars().count() <= MAX_TEXT_ARGUMENT_CHARS
                    })
                    .ok_or_else(|| {
                        format!("{name} must be a non-empty string of at most 64 characters")
                    })
            })
            .transpose()
    }

    /// Optional list of short strings, at most `max_items` long.
    ///
    /// # Errors
    /// Returns a bounded reason for a non-array, empty array, or bad element.
    pub(super) fn text_list(
        &self,
        name: &str,
        max_items: usize,
    ) -> Result<Option<Vec<&'a str>>, String> {
        let Some(value) = self.get(name) else {
            return Ok(None);
        };
        let reason = || format!("{name} must be a list of 1 through {max_items} short strings");
        let items = value.as_array().ok_or_else(reason)?;
        if items.is_empty() || items.len() > max_items {
            return Err(reason());
        }
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|text| {
                        !text.is_empty() && text.chars().count() <= MAX_TEXT_ARGUMENT_CHARS
                    })
                    .ok_or_else(reason)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// Optional instrument.
    ///
    /// # Errors
    /// Returns a bounded reason for an invalid symbol.
    pub(super) fn symbol(&self) -> Result<Option<Symbol>, String> {
        self.text("symbol")?
            .map(|symbol| {
                Symbol::parse(symbol).map_err(|_| {
                    "symbol must be 1-24 characters of letters, digits, '.', '_', '#', '+' or '-'"
                        .to_owned()
                })
            })
            .transpose()
    }

    /// The operator's presentation offset; UTC when absent.
    ///
    /// # Errors
    /// Returns a bounded reason outside −840 through 840 minutes.
    pub(super) fn operator_offset(&self) -> Result<OperatorOffset, String> {
        match self.integer("utc_offset_minutes", -840, 840)? {
            Some(minutes) => OperatorOffset::new(minutes),
            None => Ok(OperatorOffset::default()),
        }
    }

    /// Optional `since` / `until` instant in Unix milliseconds.
    ///
    /// # Errors
    /// Returns a bounded reason for an unparseable instant.
    pub(super) fn instant(
        &self,
        name: &str,
        edge: Edge,
        now_ms: i64,
        operator: OperatorOffset,
    ) -> Result<Option<i64>, String> {
        self.text(name)?
            .map(|raw| resolve_instant(name, raw, edge, now_ms, operator))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_keys_and_non_objects_are_refused() {
        let error = Args::new("closed_trades", &json!([]), &["days"]).expect_err("array");
        assert_eq!(error, "tool arguments must be an object");
        let error =
            Args::new("closed_trades", &json!({"order": 1}), &["days"]).expect_err("unknown key");
        assert!(
            error.contains("unsupported closed_trades argument `order`"),
            "{error}"
        );
        let error = Args::new("positions", &json!({"x": 1}), &[]).expect_err("no arguments");
        assert!(error.contains("does not accept arguments"), "{error}");
        assert!(Args::new("positions", &json!({}), &[]).is_ok());
    }

    #[test]
    fn values_are_typed_bounded_and_null_means_absent() {
        let arguments = json!({
            "days": 7,
            "ticket": null,
            "symbol": " usdjpy ",
            "flag": true,
            "kinds": ["proposal_evaluated"],
            "utc_offset_minutes": -300
        });
        let args = Args::new(
            "decision_history",
            &arguments,
            &[
                "days",
                "ticket",
                "symbol",
                "flag",
                "kinds",
                "utc_offset_minutes",
            ],
        )
        .expect("valid");
        assert_eq!(args.days().expect("days"), Some(7));
        assert_eq!(args.ticket().expect("null ticket"), None);
        assert_eq!(
            args.symbol().expect("symbol").expect("some").as_str(),
            "usdjpy"
        );
        assert_eq!(args.flag("flag").expect("flag"), Some(true));
        assert_eq!(
            args.text_list("kinds", 4).expect("kinds"),
            Some(vec!["proposal_evaluated"])
        );
        assert_eq!(
            args.operator_offset().expect("offset").minutes(),
            Some(-300)
        );
        assert_eq!(args.text_list("missing", 4).expect("absent"), None);

        let bad = json!({
            "days": 0,
            "ticket": -4,
            "symbol": "bad symbol!",
            "flag": "yes",
            "kinds": [],
            "utc_offset_minutes": 900,
            "since": 5
        });
        let args = Args::new(
            "decision_history",
            &bad,
            &[
                "days",
                "ticket",
                "symbol",
                "flag",
                "kinds",
                "utc_offset_minutes",
                "since",
            ],
        )
        .expect("keys are allowed");
        assert!(args.days().is_err());
        assert!(args.ticket().is_err());
        assert!(args.symbol().is_err());
        assert!(args.flag("flag").is_err());
        assert!(args.text_list("kinds", 4).is_err());
        assert!(args.operator_offset().is_err());
        assert!(
            args.instant("since", Edge::Start, 0, OperatorOffset::default())
                .is_err()
        );
        let long = json!({"kinds": ["x".repeat(65)], "symbol": "x".repeat(65)});
        let args = Args::new("decision_history", &long, &["kinds", "symbol"]).expect("keys");
        assert!(args.text_list("kinds", 4).is_err());
        assert!(args.symbol().is_err());
        let wide = json!({"kinds": ["a", "b", "c"]});
        let args = Args::new("decision_history", &wide, &["kinds"]).expect("keys");
        assert!(args.text_list("kinds", 2).is_err(), "too many items");
        let typed = json!({"kinds": "proposal_evaluated", "days": 1.5});
        let args = Args::new("decision_history", &typed, &["kinds", "days"]).expect("keys");
        assert!(args.text_list("kinds", 2).is_err(), "not a list");
        assert!(args.days().is_err(), "not an integer");
    }
}
