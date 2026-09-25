//! Live settings the console may change without a restart.
//!
//! The service reads its configuration from the environment once at startup.
//! This module puts a writable overlay in front of that read: an accepted edit
//! is stored as `NAME -> value`, persisted, and layered over the process
//! environment so every later parse sees it.
//!
//! The overlay is deliberately expressed in the same vocabulary as `.env`
//! rather than as a bespoke patch type per section. That is what lets a runtime
//! edit reuse the *existing* parser for its section verbatim: a change is
//! validated by re-parsing the whole section through the same
//! `from_source` that validates `.env`, so the console can never widen
//! behaviour beyond what a restart would accept, and no acceptance rule has to
//! be written down — or kept in step — twice.
//!
//! Two classes of setting are refused outright:
//!
//! * **Secrets** ([`is_secret`]) — an API key or token arriving over the
//!   control surface would be journaled, echoed back in status, and stored in
//!   the state table. Those stay in the environment.
//! * **Boot-only infrastructure** — a bind address, the database URL, or the
//!   deployment label cannot take effect without rebinding sockets or
//!   reconnecting pools, so accepting one would report a success that never
//!   actually happened.
//!
//! Everything else is live.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde_json::{Map, Value};

use crate::config::ConfigError;

/// Settings that may be changed at runtime.
///
/// This is an allowlist rather than a denylist: a setting added later is
/// boot-only until someone has decided what changing it mid-flight means,
/// which is the safe direction for a service that places orders.
const SETTABLE: &[&str] = &[
    // ---- Execution ----
    // The service half of the two-key execution control. The other half is
    // compiled into the EA, so this switch cannot arm live trading on its own.
    "VEYRA_TRADING_ENABLED",
    // ---- Autopilot ----
    "VEYRA_AUTOPILOT_ENABLED",
    "VEYRA_AUTOPILOT_SYMBOL",
    "VEYRA_AUTOPILOT_SYMBOLS",
    "VEYRA_AUTOPILOT_TIMEFRAME",
    "VEYRA_AUTOPILOT_BARS",
    "VEYRA_AUTOPILOT_TIER",
    "VEYRA_AUTOPILOT_INTERVAL_SECS",
    "VEYRA_AUTOPILOT_JEV",
    "VEYRA_AUTOPILOT_MIN_HOLD_SECS",
    "VEYRA_AUTOPILOT_ENTRY_MOVE_ATR",
    "VEYRA_AUTOPILOT_BREAKEVEN_R",
    "VEYRA_AUTOPILOT_TRAIL_R",
    "VEYRA_AUTOPILOT_PROFIT_HARVEST",
    "VEYRA_AUTOPILOT_HARVEST_ARM_R",
    "VEYRA_AUTOPILOT_HARVEST_TRAIL_R",
    "VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT",
    "VEYRA_AUTOPILOT_HARVEST_GIVEBACK",
    "VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS",
    "VEYRA_AUTOPILOT_HARVEST_REENTRY_COOLDOWN_SECS",
    // ---- Model ----
    "VEYRA_MODEL_PROVIDER",
    "VEYRA_MODEL_BASE_URL",
    "VEYRA_MODEL_FAST",
    "VEYRA_MODEL_BALANCED",
    "VEYRA_MODEL_REASONING",
    "VEYRA_MODEL_FALLBACKS",
    "VEYRA_MODEL_FAST_FALLBACKS",
    "VEYRA_MODEL_BALANCED_FALLBACKS",
    "VEYRA_MODEL_REASONING_FALLBACKS",
    "VEYRA_MODEL_COMPEL_STRUCTURED",
    "VEYRA_MODEL_HTTP_REFERER",
    "VEYRA_MODEL_APP_TITLE",
    "VEYRA_MODEL_APP_HIDDEN",
    "VEYRA_MODEL_MAX_CALLS_PER_HOUR",
    "VEYRA_MODEL_MAX_CALLS_PER_DAY",
    // Routing only: a connected ChatGPT subscription first, with its model.
    "VEYRA_MODEL_PREFER_SUBSCRIPTION",
    "VEYRA_MODEL_CHATGPT_MODEL",
    // ---- Jev ----
    "VEYRA_JEV_PROVIDER",
    "VEYRA_JEV_BASE_URL",
    "VEYRA_JEV_MODEL",
    // ---- Market data ----
    "VEYRA_MARKET_PROVIDER",
    "VEYRA_MARKET_EA_AWAIT_SECS",
    // ---- Housekeeping ----
    "VEYRA_RECONCILE_SECS",
    "VEYRA_AUDIT_RETENTION_DAYS",
];

/// Whether a setting carries a credential.
///
/// Matched on shape rather than by listing names, so a credential added later
/// is refused by default instead of relying on this module being updated.
pub fn is_secret(name: &str) -> bool {
    name.ends_with("_API_KEY") || name.ends_with("_TOKEN") || name.ends_with("_SECRET")
}

/// Whether a setting may be changed at runtime.
pub fn is_settable(name: &str) -> bool {
    !is_secret(name) && SETTABLE.contains(&name)
}

/// Why a proposed edit was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct RejectedEdit {
    /// Setting the operator tried to change.
    pub name: String,
    /// Non-sensitive acceptance rule.
    pub reason: String,
}

impl RejectedEdit {
    fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reason: reason.into(),
        }
    }
}

impl From<ConfigError> for RejectedEdit {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::MissingEnvironmentVariable { name } => Self::new(name, "must be supplied"),
            ConfigError::InvalidEnvironmentVariable { name, reason } => Self::new(name, reason),
        }
    }
}

/// The live overlay in front of the process environment.
///
/// Cloning shares the same overlay: every holder observes an accepted edit.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    overrides: Arc<RwLock<BTreeMap<String, String>>>,
    /// Decrypted console credential, never included in snapshots or status.
    model_key: Arc<RwLock<Option<crate::model::ApiKey>>>,
}

impl RuntimeConfig {
    /// An empty overlay: every setting reads through to the environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves one setting, preferring an override over the environment.
    ///
    /// An override holding the empty string is *not* the same as an absent
    /// one: it means "explicitly cleared", which is how a section's optional
    /// values are returned to their defaults. Parsers already treat blank as
    /// unset, so this needs no special case downstream.
    pub fn resolve(&self, name: &'static str) -> Result<String, ConfigError> {
        if name == "VEYRA_MODEL_API_KEY"
            && let Some(key) = self
                .model_key
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
        {
            return Ok(key.expose().to_owned());
        }
        if let Some(value) = self.read().get(name) {
            return Ok(value.clone());
        }
        std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
    }

    /// A source function suitable for any section's `from_source` parser.
    pub fn source(&self) -> impl FnMut(&'static str) -> Result<String, ConfigError> + use<> {
        let snapshot = self.read().clone();
        let model_key = self
            .model_key
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        move |name| {
            if name == "VEYRA_MODEL_API_KEY"
                && let Some(key) = &model_key
            {
                return Ok(key.expose().to_owned());
            }
            if let Some(value) = snapshot.get(name) {
                return Ok(value.clone());
            }
            std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
        }
    }

    /// A source function layering `pending` over the current overlay, used to
    /// validate a proposed edit before it is committed.
    pub fn trial_source(
        &self,
        pending: &BTreeMap<String, String>,
    ) -> impl FnMut(&'static str) -> Result<String, ConfigError> + use<> {
        let mut snapshot = self.read().clone();
        let model_key = self
            .model_key
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for (name, value) in pending {
            snapshot.insert(name.clone(), value.clone());
        }
        move |name| {
            if name == "VEYRA_MODEL_API_KEY"
                && let Some(key) = &model_key
            {
                return Ok(key.expose().to_owned());
            }
            if let Some(value) = snapshot.get(name) {
                return Ok(value.clone());
            }
            std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
        }
    }

    /// Installs or removes a separately persisted credential. This value is
    /// intentionally absent from the ordinary settings overlay and audit.
    pub fn set_model_key(&self, key: Option<crate::model::ApiKey>) {
        *self
            .model_key
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = key;
    }

    /// Non-sensitive credential status for the console.
    pub fn model_key_status(&self) -> Value {
        let key = self
            .model_key
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(key) = key.as_ref() {
            let hint: String = key
                .expose()
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            return serde_json::json!({"set": true, "source": "console", "hint": hint});
        }
        if let Ok(value) = std::env::var("VEYRA_MODEL_API_KEY")
            && !value.trim().is_empty()
        {
            let hint: String = value
                .trim()
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            return serde_json::json!({"set": true, "source": "environment", "hint": hint});
        }
        serde_json::json!({"set": false, "source": null, "hint": null})
    }

    /// Screens a proposed edit for names that may not be set at all.
    ///
    /// This is the gate on *which* settings are live. Whether the values are
    /// acceptable is decided afterwards, by re-parsing each affected section
    /// through its own parser.
    ///
    /// # Errors
    /// Returns every refusal at once rather than the first, so a console form
    /// can mark all its bad fields in one round trip.
    pub fn screen(
        &self,
        patch: &Map<String, Value>,
    ) -> Result<BTreeMap<String, String>, Vec<RejectedEdit>> {
        let mut accepted = BTreeMap::new();
        let mut rejected = Vec::new();

        for (name, value) in patch {
            if is_secret(name) {
                rejected.push(RejectedEdit::new(
                    name,
                    "credentials are read from the environment and cannot be set at runtime",
                ));
                continue;
            }
            if !SETTABLE.contains(&name.as_str()) {
                rejected.push(RejectedEdit::new(
                    name,
                    "not a runtime-settable setting; it takes effect only at startup",
                ));
                continue;
            }
            // Null clears an override, returning the setting to whatever the
            // environment says — the only way back to the startup baseline.
            let text = match value {
                Value::Null => String::new(),
                Value::String(text) => text.trim().to_owned(),
                Value::Bool(flag) => flag.to_string(),
                Value::Number(number) => number.to_string(),
                _ => {
                    rejected.push(RejectedEdit::new(
                        name,
                        "must be a string, number, boolean, or null",
                    ));
                    continue;
                }
            };
            accepted.insert(name.clone(), text);
        }

        if rejected.is_empty() {
            Ok(accepted)
        } else {
            Err(rejected)
        }
    }

    /// Commits an already-validated edit.
    pub fn commit(&self, accepted: BTreeMap<String, String>) {
        let mut overrides = self.write();
        for (name, value) in accepted {
            overrides.insert(name, value);
        }
    }

    /// Replaces the whole overlay, used when restoring persisted state.
    ///
    /// Unknown or no-longer-settable names are dropped rather than refused: a
    /// row written by an older build must not stop this one from starting.
    pub fn restore(&self, stored: &Value) -> usize {
        let Some(object) = stored.as_object() else {
            return 0;
        };
        let mut overrides = self.write();
        overrides.clear();
        let mut restored = 0;
        for (name, value) in object {
            if !is_settable(name) {
                continue;
            }
            if let Some(text) = value.as_str() {
                overrides.insert(name.clone(), text.to_owned());
                restored += 1;
            }
        }
        restored
    }

    /// The overlay as a persistable snapshot.
    pub fn snapshot(&self) -> Value {
        Value::Object(
            self.read()
                .iter()
                .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                .collect(),
        )
    }

    /// The effective value of every settable setting, for the console.
    ///
    /// `overridden` distinguishes a value the operator set from one still
    /// coming from the environment, so the console can show what has drifted
    /// from the deployed baseline.
    pub fn effective(&self) -> Value {
        let overrides = self.read();
        let mut fields = Map::new();
        for name in SETTABLE {
            let overridden = overrides.get(*name);
            let value = overridden
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .unwrap_or_default();
            fields.insert(
                (*name).to_owned(),
                serde_json::json!({
                    "value": value,
                    "overridden": overridden.is_some(),
                }),
            );
        }
        Value::Object(fields)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, String>> {
        self.overrides
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, String>> {
        self.overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Sections whose edits take effect without a restart.
///
/// Everything else in [`SETTABLE`] is stored and honoured at the next start.
/// Saying so explicitly is the point: silently accepting a change that does
/// not apply is worse than refusing it, because the operator believes the
/// setting moved.
pub const LIVE_SECTIONS: &[&str] = &["execution", "autopilot", "model"];

/// Sections re-parsed and ready to install, produced by [`validate`].
///
/// Holding the parsed values between validation and installation is what makes
/// the update all-or-nothing: by the time anything is swapped, every section
/// has already proven it parses.
pub struct StagedSettings {
    trading_enabled: bool,
    autopilot: Option<crate::trading::autopilot::AutopilotSettings>,
    model: Option<crate::model::ModelSettings>,
}

/// Re-parses every live section against the proposed overlay.
///
/// # Errors
/// Returns the first setting a section's own parser refused, with that
/// parser's reason verbatim.
pub fn validate(
    config: &RuntimeConfig,
    pending: &BTreeMap<String, String>,
) -> Result<StagedSettings, RejectedEdit> {
    let trading_enabled = match config.trial_source(pending)("VEYRA_TRADING_ENABLED")
        .unwrap_or_default()
        .trim()
    {
        "" | "false" => false,
        "true" => true,
        _ => {
            return Err(RejectedEdit::new(
                "VEYRA_TRADING_ENABLED",
                "must be `true` or `false`",
            ));
        }
    };

    let autopilot =
        crate::trading::autopilot::AutopilotSettings::from_source(config.trial_source(pending))?;
    let model = crate::model::ModelSettings::from_source(config.trial_source(pending))?;

    Ok(StagedSettings {
        trading_enabled,
        autopilot,
        model,
    })
}

/// Installs already-validated settings into the running service.
///
/// Model cooldowns are cleared when the new settings route different
/// candidates (provider, credential, base URL, model lists, structured-answer
/// mode, or the ChatGPT preference): what a cooldown said about the old route
/// is no longer evidence about the new one. An edit that leaves the route
/// alone keeps them, so it cannot be used to reset a cooldown.
///
/// # Errors
/// Returns a description when the model provider refuses to be rebuilt. The
/// other sections cannot fail at this point: they are plain values.
pub fn adopt(state: &crate::AppState, staged: StagedSettings) -> Result<(), String> {
    // The model engine is the only section that has to be reconstructed, so it
    // goes first: if the provider refuses the new settings, nothing else has
    // been disturbed yet.
    let previous = state.model();
    let rebuilt = match staged.model {
        None => None,
        Some(settings) => Some(build_model(state, settings, previous.as_ref())?),
    };
    let route_changed = route_changed(previous.as_ref(), rebuilt.as_ref());

    state.set_trading_enabled(staged.trading_enabled);
    state.set_autopilot(staged.autopilot);
    state.set_model(rebuilt);
    if route_changed {
        state
            .model_cooldowns()
            .clear("model route settings changed");
    }
    Ok(())
}

/// Rebuilds only the model runtime from the current overlay — used when the
/// ChatGPT subscription connects or disconnects, which changes the route
/// without any setting changing. Trading and autopilot are left untouched.
///
/// # Errors
/// Returns a description when the settings no longer parse or the provider
/// refuses to be rebuilt; the running model is left in place.
pub fn rebuild_model(state: &crate::AppState) -> Result<(), String> {
    let settings = crate::model::ModelSettings::from_source(state.runtime_config().source())
        .map_err(|error| RejectedEdit::from(error).reason)?;
    let previous = state.model();
    let rebuilt = match settings {
        None => None,
        Some(settings) => Some(build_model(state, settings, previous.as_ref())?),
    };
    state.set_model(rebuilt);
    Ok(())
}

/// Builds a runtime and carries the previous call-budget windows across.
fn build_model(
    state: &crate::AppState,
    settings: crate::model::ModelSettings,
    previous: Option<&crate::model::ModelRuntime>,
) -> Result<crate::model::ModelRuntime, String> {
    let runtime = crate::model::ModelRuntime::from_settings_with_app(settings, state)
        .map_err(|error| error.to_string())?;
    // A rebuilt runtime starts with empty call-budget windows. Carrying the
    // old counters across means an edit cannot be used — even accidentally —
    // to reset a cap that exists to bound spend.
    if let Some(previous) = previous {
        let carried = previous.state_snapshot();
        if let Err(error) = runtime.restore_state(&carried) {
            tracing::warn!(%error, "model call budget could not be carried across a settings change");
        }
    }
    Ok(runtime)
}

/// Whether the installed route changes between two runtimes.
fn route_changed(
    previous: Option<&crate::model::ModelRuntime>,
    next: Option<&crate::model::ModelRuntime>,
) -> bool {
    match (previous, next) {
        (None, None) => false,
        (Some(previous), Some(next)) => !previous.same_route(next),
        _ => true,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Overlay fixtures shared by crate tests.

    use std::collections::BTreeMap;

    use super::RuntimeConfig;

    /// Every model setting pinned in the overlay, so the process environment
    /// cannot leak into these tests.
    pub(crate) fn model_overlay(config: &RuntimeConfig, overrides: &[(&str, &str)]) {
        let mut pinned: BTreeMap<String, String> = [
            ("VEYRA_MODEL_PROVIDER", "openrouter"),
            ("VEYRA_MODEL_BASE_URL", ""),
            ("VEYRA_MODEL_FAST", "deepseek/deepseek-v4.1-flash"),
            ("VEYRA_MODEL_BALANCED", "deepseek/deepseek-v4.1-flash"),
            ("VEYRA_MODEL_REASONING", "deepseek/deepseek-v4.1-flash"),
            ("VEYRA_MODEL_FALLBACKS", "z-ai/glm-5.3-flash"),
            ("VEYRA_MODEL_FAST_FALLBACKS", ""),
            ("VEYRA_MODEL_BALANCED_FALLBACKS", ""),
            ("VEYRA_MODEL_REASONING_FALLBACKS", ""),
            ("VEYRA_MODEL_COMPEL_STRUCTURED", ""),
            ("VEYRA_MODEL_HTTP_REFERER", ""),
            ("VEYRA_MODEL_APP_TITLE", ""),
            ("VEYRA_MODEL_APP_HIDDEN", ""),
            ("VEYRA_MODEL_MAX_CALLS_PER_HOUR", ""),
            ("VEYRA_MODEL_MAX_CALLS_PER_DAY", ""),
            ("VEYRA_MODEL_PREFER_SUBSCRIPTION", ""),
            ("VEYRA_MODEL_CHATGPT_MODEL", ""),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        for (name, value) in overrides {
            pinned.insert((*name).to_owned(), (*value).to_owned());
        }
        config.commit(pinned);
        config.set_model_key(Some(
            crate::model::ApiKey::parse("test-key-12345678").expect("valid key"),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::model_overlay;
    use super::*;
    use serde_json::json;

    #[test]
    fn separately_stored_key_is_usable_but_absent_from_config_snapshots() {
        let config = RuntimeConfig::new();
        let key = crate::model::ApiKey::parse("private-provider-key").expect("valid test key");
        config.set_model_key(Some(key));
        assert_eq!(
            config.source()("VEYRA_MODEL_API_KEY").expect("key"),
            "private-provider-key"
        );
        assert_eq!(
            config.trial_source(&BTreeMap::new())("VEYRA_MODEL_API_KEY").expect("key"),
            "private-provider-key"
        );
        assert!(
            !config
                .snapshot()
                .to_string()
                .contains("private-provider-key")
        );
        assert!(
            !config
                .effective()
                .to_string()
                .contains("private-provider-key")
        );
        assert_eq!(config.model_key_status()["source"], "console");
    }

    fn patch(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn credentials_are_refused_however_they_are_spelled() {
        let config = RuntimeConfig::new();
        let rejected = config
            .screen(&patch(&[
                ("VEYRA_MODEL_API_KEY", json!("sk-or-stolen")),
                ("VEYRA_EA_TOKEN", json!("token")),
            ]))
            .expect_err("credentials never cross the control surface");

        assert_eq!(rejected.len(), 2);
        assert!(
            rejected
                .iter()
                .all(|edit| edit.reason.contains("credentials")),
            "both refusals must say why: {rejected:?}"
        );
    }

    #[test]
    fn boot_only_infrastructure_is_refused() {
        let config = RuntimeConfig::new();
        for name in [
            "VEYRA_BIND_PORT",
            "VEYRA_DATABASE_URL",
            "VEYRA_ENV",
            "VEYRA_EA_BIND_HOST",
        ] {
            let rejected = config
                .screen(&patch(&[(name, json!("whatever"))]))
                .expect_err("{name} cannot take effect without a restart");
            assert_eq!(rejected[0].name, name);
        }
    }

    #[test]
    fn accepted_edits_shadow_the_environment() {
        let config = RuntimeConfig::new();
        let accepted = config
            .screen(&patch(&[("VEYRA_AUTOPILOT_INTERVAL_SECS", json!("60"))]))
            .expect("a live setting");
        config.commit(accepted);

        assert_eq!(
            config
                .resolve("VEYRA_AUTOPILOT_INTERVAL_SECS")
                .expect("resolved"),
            "60"
        );
    }

    #[test]
    fn scalars_are_accepted_in_their_natural_json_shape() {
        let config = RuntimeConfig::new();
        let accepted = config
            .screen(&patch(&[
                ("VEYRA_AUTOPILOT_PROFIT_HARVEST", json!(true)),
                ("VEYRA_AUTOPILOT_INTERVAL_SECS", json!(90)),
            ]))
            .expect("a console sends real JSON types, not only strings");

        assert_eq!(accepted["VEYRA_AUTOPILOT_PROFIT_HARVEST"], "true");
        assert_eq!(accepted["VEYRA_AUTOPILOT_INTERVAL_SECS"], "90");
    }

    #[test]
    fn null_clears_an_override_back_to_the_environment_baseline() {
        let config = RuntimeConfig::new();
        config.commit(
            config
                .screen(&patch(&[("VEYRA_AUTOPILOT_TRAIL_R", json!("1.5"))]))
                .expect("a live setting"),
        );
        assert_eq!(config.resolve("VEYRA_AUTOPILOT_TRAIL_R").unwrap(), "1.5");

        config.commit(
            config
                .screen(&patch(&[("VEYRA_AUTOPILOT_TRAIL_R", Value::Null)]))
                .expect("null is how a value is cleared"),
        );
        assert_eq!(
            config.resolve("VEYRA_AUTOPILOT_TRAIL_R").unwrap(),
            "",
            "a cleared override reads as unset, which every parser treats as its default"
        );
    }

    #[test]
    fn every_refusal_is_reported_in_one_round_trip() {
        let config = RuntimeConfig::new();
        let rejected = config
            .screen(&patch(&[
                ("VEYRA_MODEL_API_KEY", json!("sk")),
                ("VEYRA_BIND_PORT", json!("9000")),
                ("VEYRA_AUTOPILOT_TRAIL_R", json!({"nested": true})),
            ]))
            .expect_err("three bad fields");

        assert_eq!(
            rejected.len(),
            3,
            "a console form marks all its bad fields at once: {rejected:?}"
        );
    }

    #[test]
    fn a_restored_snapshot_drops_names_this_build_no_longer_accepts() {
        let config = RuntimeConfig::new();
        let restored = config.restore(&json!({
            "VEYRA_AUTOPILOT_INTERVAL_SECS": "120",
            "VEYRA_RETIRED_SETTING": "whatever",
            "VEYRA_MODEL_API_KEY": "sk-or-leaked",
        }));

        assert_eq!(restored, 1, "only the still-settable name survives");
        assert_eq!(
            config.resolve("VEYRA_AUTOPILOT_INTERVAL_SECS").unwrap(),
            "120"
        );
        assert!(
            config.snapshot()["VEYRA_MODEL_API_KEY"].is_null(),
            "a credential in an old row is never adopted"
        );
    }

    #[test]
    fn the_snapshot_round_trips_through_restore() {
        let config = RuntimeConfig::new();
        config.commit(
            config
                .screen(&patch(&[
                    ("VEYRA_AUTOPILOT_PROFIT_HARVEST", json!("true")),
                    ("VEYRA_MODEL_FALLBACKS", json!("z-ai/glm-5.3-flash")),
                ]))
                .expect("live settings"),
        );

        let restored = RuntimeConfig::new();
        restored.restore(&config.snapshot());
        assert_eq!(restored.snapshot(), config.snapshot());
    }

    fn cool_one(state: &crate::AppState) {
        if let crate::model::Admission::Ready(ticket) = state.model_cooldowns().admit(
            crate::model::ModelProvider::OpenRouter,
            "z-ai/glm-5.3-flash",
        ) {
            ticket.fail(crate::model::CooldownFailure::new(
                crate::model::CooldownReason::ProviderRejected,
            ));
        }
        assert_eq!(state.model_cooldowns().len(), 1);
    }

    #[test]
    fn the_chatgpt_preference_is_live_and_settable() {
        for name in [
            "VEYRA_MODEL_PREFER_SUBSCRIPTION",
            "VEYRA_MODEL_CHATGPT_MODEL",
        ] {
            assert!(is_settable(name), "{name}");
            assert!(
                RuntimeConfig::new().effective().get(name).is_some(),
                "{name}"
            );
        }
        let config = RuntimeConfig::new();
        model_overlay(&config, &[]);
        let rejected = validate(
            &config,
            &[(
                "VEYRA_MODEL_PREFER_SUBSCRIPTION".to_owned(),
                "sometimes".to_owned(),
            )]
            .into_iter()
            .collect(),
        )
        .err()
        .expect("a malformed switch is refused");
        assert_eq!(rejected.name, "VEYRA_MODEL_PREFER_SUBSCRIPTION");
    }

    #[actix_web::test]
    async fn adopting_a_new_route_clears_cooldowns_but_a_cosmetic_edit_does_not() {
        use crate::model::subscription_engine::test_support::app;
        let (state, _store) = app(crate::model::CooldownRegistry::new(), &[]);
        model_overlay(state.runtime_config(), &[]);
        adopt(
            &state,
            validate(state.runtime_config(), &BTreeMap::new()).expect("valid"),
        )
        .expect("adopts");
        assert!(state.model().is_some());
        cool_one(&state);

        // Budget and attribution do not change what a cooldown says.
        let cosmetic: BTreeMap<String, String> = [(
            "VEYRA_MODEL_MAX_CALLS_PER_HOUR".to_owned(),
            "500".to_owned(),
        )]
        .into_iter()
        .collect();
        adopt(
            &state,
            validate(state.runtime_config(), &cosmetic).expect("valid"),
        )
        .expect("adopts");
        assert_eq!(
            state.model_cooldowns().len(),
            1,
            "kept across a cosmetic edit"
        );

        // A different model list is a different route.
        let routed: BTreeMap<String, String> = [(
            "VEYRA_MODEL_FALLBACKS".to_owned(),
            "xiaomi/mimo-v2.6-flash".to_owned(),
        )]
        .into_iter()
        .collect();
        adopt(
            &state,
            validate(state.runtime_config(), &routed).expect("valid"),
        )
        .expect("adopts");
        assert!(state.model_cooldowns().is_empty());

        // Disabling the model is a route change too.
        cool_one(&state);
        assert!(route_changed(state.model().as_ref(), None));
        assert!(!route_changed(None, None));
    }

    #[actix_web::test]
    async fn a_model_rebuild_follows_the_chatgpt_connection() {
        use crate::model::subscription_engine::test_support::{app, credential};
        use crate::subscription_auth::SubscriptionProvider;
        let (state, _store) = app(crate::model::CooldownRegistry::new(), &[]);
        model_overlay(state.runtime_config(), &[]);
        rebuild_model(&state).expect("rebuilds");
        let plain = state.model().expect("configured");
        assert!(!plain.prefers_subscription());

        state
            .subscription_auth()
            .set_credential(credential(SubscriptionProvider::Codex, None));
        rebuild_model(&state).expect("rebuilds");
        let preferred = state.model().expect("configured");
        assert!(preferred.prefers_subscription());
        assert_eq!(
            preferred.route(crate::model::ModelTier::Fast),
            [
                "chatgpt:gpt-6-luna",
                "deepseek/deepseek-v4.1-flash",
                "z-ai/glm-5.3-flash"
            ]
        );

        // Settings that no longer parse leave the running model alone.
        state.runtime_config().commit(
            [("VEYRA_MODEL_FAST".to_owned(), "no-vendor".to_owned())]
                .into_iter()
                .collect(),
        );
        assert!(rebuild_model(&state).is_err());
        assert!(
            state
                .model()
                .is_some_and(|current| current.same_instance(&preferred))
        );
    }
}
