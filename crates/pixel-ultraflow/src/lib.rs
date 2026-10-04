//! pixel-ultraflow — the classify-driven browser loop over saved flows.
//!
//! The loop borrowed from `browser-use/jev-ultrafast`, with `pixel classify`
//! where its decision model would be and `pixel flow`'s saved JSON where its
//! plan memory would be. Two halves:
//!
//! - **Discovery.** One question per cycle whose options *are* the
//!   operation-target pairs the page offers ([`action::ActionSpace`]), so one
//!   answer is one executable action. The run records what happened; when it
//!   completes, [`compose`] turns the traces into a `pixel_flow::Flow`, which
//!   `pixel flow` can then list, show, revise and run like any other
//!   flow.
//! - **Replay.** The saved flow is followed step by step ([`replay`]).
//!   Its `conditional` steps are the *different conditions* of how to do the
//!   task: each one is a classify question about the current page, not a
//!   substring match, with the text matcher disclosed as the fallback. A step
//!   whose page no longer matches is re-decided once, reported as a deviation,
//!   and handed back as a step the caller can record.
//!
//! The decision engine is a seam ([`decide::Decider`]) and so is the browser
//! ([`pixel_flow::Browser`]): production wires `pixel classify` and
//! `agent-browser`, and the tests script both, so the whole loop is exercised
//! without a model, a network or a page.

pub mod action;
pub mod compose;
pub mod decide;
pub mod discover;
pub mod elements;
pub mod replay;
pub mod twostage;
pub mod value;

pub use action::{Action, ActionSpace, Choice, Op};
pub use compose::{Composed, FlowMeta, compose};
pub use decide::{Decider, Decision, Distribution};
pub use discover::{
    Cycle, DecisionRecord, DiscoverRequest, Limits, Status, Trace, TracedStep, TwoStageChoice,
    discover, one_cycle,
};
pub use elements::{Element, Observation};
pub use replay::{ConditionRecord, Deviation, ReplayReport, ReplayRequest, replay};
pub use value::{Resolved, ValueChoice, ValueSource, Var};

#[cfg(test)]
pub(crate) mod testutil;
