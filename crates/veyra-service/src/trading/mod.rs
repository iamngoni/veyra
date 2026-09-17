//! Trade-intent domain boundary.
//!
//! [`intent`] owns the only shape a strategy may propose; [`pipeline`] runs
//! one structured model answer through the deterministic risk gate. Nothing
//! here reaches a broker: approvals still require the command layer, and no
//! execution path exists yet.

pub mod intent;
pub mod pipeline;

pub use intent::{
    Comment, IntentError, IntentId, OrderKind, Price, Side, TradeIntent, TradeIntentDraft,
    TradeProposal, Volume, parse_instrument,
};
pub use pipeline::{
    PROPOSAL_SCHEMA_NAME, PipelineError, PipelineOutcome, evaluate_proposal, proposal_format,
};
