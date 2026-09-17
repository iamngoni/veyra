//! Trade-intent domain boundary.
//!
//! [`intent`] owns the only shape a strategy may propose; [`pipeline`] runs
//! one structured model answer through the deterministic risk gate; and
//! [`autopilot`] drives that pipeline on a cadence, handing approved intents
//! to the single staged execution path (which still requires both operator
//! controls).

pub mod autopilot;
pub mod intent;
pub mod pipeline;

pub use autopilot::{AutopilotSettings, JevPreference, TickOutcome};
pub use intent::{
    Comment, IntentError, IntentId, OrderKind, Price, Side, TradeIntent, TradeIntentDraft,
    TradeProposal, Volume, parse_instrument,
};
pub use pipeline::{
    PROPOSAL_SCHEMA_NAME, PipelineError, PipelineOutcome, evaluate_proposal, proposal_format,
};
