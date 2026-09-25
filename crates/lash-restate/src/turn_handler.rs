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
//!
//! [`parked_turn_failure`] is the one way a handler ends a parked attempt.
//! It is a retryable failure, because nothing else can end it: a park is
//! found while the attempt still replays its journal, and the SDK refuses
//! every new command during replay, the handler's output included. A handler
//! that returned, or failed terminally, at the park would propose an output
//! where the journal holds its next command, which Restate refuses as a
//! journal mismatch (FIG-3697); a retryable failure writes nothing.

use restate_sdk::endpoint::{HandlerOptions, ServiceOptions};
use restate_sdk::errors::HandlerError;
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

/// How a handler ends an attempt whose lash turn — or process segment —
/// parked on a replay divergence ([`TurnFailureCause::Parked`](lash_core::TurnFailureCause::Parked)):
/// a retryable failure, never an early return and never a terminal error.
///
/// The park row is already durable when the handler sees the refusal. The
/// failure keeps the invocation and its journal; the handler's retry policy
/// ([`turn_handler_options`]) pauses it after its last attempt, and a retry
/// under the restored build replays the journal and completes the turn once.
pub fn parked_turn_failure(refusal: impl std::fmt::Display) -> HandlerError {
    HandlerError::from(ParkedTurn(refusal.to_string()))
}

/// Records the park of the turn a group tool child belongs to, when the child
/// refused where it parks its opener and recorded nothing (FIG-3725): its
/// tool drifted and it would run live, or its replay diverged. Its opener,
/// suspended on the child's rank, cannot learn of a refusal that settles
/// nothing, so the child writes the turn's typed park through the session's
/// own store, where the parked-work surface shows it. `None` when the child's
/// scope names no turn (a process opener, a queue drain with no attributed
/// turn), or its session has no store.
pub(crate) async fn park_refused_group_child(
    sessions: &dyn lash_core::SessionStoreFactory,
    scope: &lash_core::ExecutionScope,
    physical_turn: Option<&lash_core::TurnId>,
    refusal: &lash_core::RuntimeEffectControllerError,
) -> Result<Option<lash_core::store::TurnPark>, lash_core::StoreError> {
    use lash_core::ClockWallTime as _;
    lash_core::park_turn_of_refused_group_child(
        sessions,
        scope,
        physical_turn,
        &refusal.clone().into_runtime_error(),
        lash_core::facade_support::SystemClock.timestamp_ms(),
    )
    .await
}

/// The retryable failure [`parked_turn_failure`] ends a parked attempt with.
#[derive(Debug)]
struct ParkedTurn(String);

impl std::fmt::Display for ParkedTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parked on a replay divergence: {}", self.0)
    }
}

impl std::error::Error for ParkedTurn {}

/// Parks the in-flight turn of `scope` whose redrive the session-state
/// generation gate refused, and returns the park (FIG-3735).
///
/// A turn handler calls it when its turn fails with a generation refusal
/// (the facade's `EmbedError::session_state_version_refusal`). A
/// scope with a turn in flight — a queue drain's unsettled run, a direct
/// turn's journaled acceptance — ran under an earlier execution whose journal
/// already holds commands: the handler ends the attempt with
/// [`parked_turn_failure`], so the invocation keeps its journal and pauses
/// after its attempt budget, and the typed park row names both generations. A
/// scope with nothing in flight answers `None`: nothing was journaled, and
/// the refusal is the turn's terminal answer. A store failure is the store's,
/// and the handler retries it.
pub async fn park_generation_refused_turn(
    sessions: &dyn lash_core::SessionStoreFactory,
    scope: &lash_core::ExecutionScope,
    refusal: lash_core::SessionStateVersionRefusal,
    at_ms: u64,
) -> Result<Option<lash_core::store::TurnPark>, lash_core::StoreError> {
    let session_id = match scope {
        lash_core::ExecutionScope::QueueDrain { session_id, .. }
        | lash_core::ExecutionScope::Turn { session_id, .. } => session_id,
        _ => return Ok(None),
    };
    let Some(store) = sessions.open_existing_store_by_id(session_id).await? else {
        return Ok(None);
    };
    lash_core::park_turn_refused_by_generation(store.as_ref(), scope, refusal, at_ms).await
}
