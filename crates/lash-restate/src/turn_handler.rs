//! The retry contract of a handler that runs a lash turn.
//!
//! A turn that parks ([`RuntimeErrorCode::parks_turn`](lash_core::RuntimeErrorCode::parks_turn),
//! for one a redrive whose journal diverged,
//! [`EffectReplayDivergence`](lash_core::RuntimeErrorCode::EffectReplayDivergence))
//! has written its park through the session store and holds its claims. Its
//! handler fails the attempt *retryably*, never with a `TerminalError`, so the
//! invocation keeps its journal: a later attempt on a restored build replays
//! it and completes the turn once. The handler neither settles the turn nor
//! records a failure.
//!
//! Every attempt of a diverged invocation refuses the same way, so a turn
//! handler bounds them: after [`TURN_HANDLER_MAX_ATTEMPTS`] the invocation
//! pauses instead of burning retries, keeping its journal for an operator to
//! resume, cancel, or fork.

use restate_sdk::endpoint::{HandlerOptions, ServiceOptions};
use restate_sdk::service::{IntoServiceDefinition, ServiceDefinition};

/// Attempts an invocation of a turn handler makes before it pauses. It
/// mirrors the in-process driver's transient attempt budget.
pub const TURN_HANDLER_MAX_ATTEMPTS: u64 = 8;

/// The options of a handler that runs a lash turn: at most
/// [`TURN_HANDLER_MAX_ATTEMPTS`] attempts, then pause.
pub fn turn_handler_options() -> HandlerOptions {
    HandlerOptions::new()
        .retry_policy_max_attempts(TURN_HANDLER_MAX_ATTEMPTS)
        .retry_policy_pause_on_max_attempts()
}

/// `definition` with its `handler` configured as one that runs a lash turn
/// ([`turn_handler_options`]), ready to bind.
pub fn turn_service(definition: impl IntoServiceDefinition, handler: &str) -> ServiceDefinition {
    definition
        .into_service_definition()
        .options(ServiceOptions::new().handler(handler, turn_handler_options()))
}
