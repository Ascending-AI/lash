//! The generation sentinel (ADR 0106 §1, FIG-3795).
//!
//! Every lash handler whose journal only its own build may replay — the
//! process segment workflow, the effect-group dispatcher's run and children,
//! and the session driver's `LashSession` and `LashTurn` — records the
//! executing build's drain generation `G` as its journal's first command: a
//! `ctx.run` step named [`GENERATION_SENTINEL`] whose recorded output is
//! the generation as a JSON string.
//!
//! On a replay the recorded value comes back. When it names another
//! generation, the code behind the invocation's pinned deployment was
//! swapped (ADR 0043 forbids it, and nothing else can reach this), so the
//! handler parks its attempt, typed with the recorded generation, before it
//! replays any other command: the journal is kept for a build of that
//! generation, and no effect of the other build's code ever runs against
//! it.
//!
//! The step's name and its output encoding are frozen: every later
//! generation must read a sentinel any earlier one wrote.

use lash_core::engine::BuildGeneration;
use restate_sdk::errors::HandlerError;

/// The journal name of the sentinel step. Frozen.
pub(crate) const GENERATION_SENTINEL: &str = "lash.build.generation";

/// Journal `executing` (a [`BuildGeneration`]) as the handler's first
/// command on `ctx` (a Restate handler context), and evaluate to the
/// generation the journal holds: `executing` on a first run, the recorded
/// generation on a replay, as `Result<BuildGeneration, TerminalError>`.
///
/// A macro rather than a generic function: a helper generic over the
/// context's lifetime makes the SDK's handler futures fail their
/// higher-ranked `Send` bound, and the step is small enough to expand in
/// place.
macro_rules! record_generation {
    ($ctx:expr, $executing:expr) => {{
        let executing: ::lash_core::engine::BuildGeneration =
            ::std::clone::Clone::clone($executing);
        ::restate_sdk::context::RunFuture::name(
            ::restate_sdk::context::ContextSideEffects::run($ctx, move || async move {
                Ok(::restate_sdk::serde::Json(executing))
            }),
            $crate::sentinel::GENERATION_SENTINEL,
        )
        .await
        .map(|::restate_sdk::serde::Json(recorded)| recorded)
    }};
}
pub(crate) use record_generation;

/// The sentinel's verdict for `handler`: `Ok` when the journal was recorded
/// by this build's generation, the typed `RetiredGeneration` park failure
/// otherwise — retryable, so the invocation keeps its journal and pauses
/// after its attempt budget for a build of the recorded generation.
pub(crate) fn check_generation(
    handler: &str,
    recorded: &BuildGeneration,
    executing: &BuildGeneration,
) -> Result<(), HandlerError> {
    if recorded == executing {
        return Ok(());
    }
    Err(crate::parked_turn_failure(retired_generation_message(
        handler, recorded, executing,
    )))
}

/// What a `RetiredGeneration` refusal of a journal says: the handler, the
/// generation that recorded it, and the build refusing it.
pub(crate) fn retired_generation_message(
    handler: &str,
    recorded: &BuildGeneration,
    executing: &BuildGeneration,
) -> String {
    format!(
        "RetiredGeneration: {handler} journal was recorded under generation `{}`; this build \
         is generation `{}` and parks it for a build of the recorded generation",
        recorded.as_str(),
        executing.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_admits_only_its_own_generation() {
        let own = BuildGeneration::for_test("t0");
        assert!(check_generation("LashTurn", &own, &own).is_ok());
        let other = BuildGeneration::for_test("t1");
        let refusal =
            check_generation("LashTurn", &other, &own).expect_err("another generation parks");
        let message = format!("{refusal:?}");
        assert!(message.contains("RetiredGeneration"), "{message}");
        assert!(message.contains(other.as_str()), "{message}");
    }

    /// The sentinel's journaled bytes are frozen: the step name, and an
    /// output that is the generation as a bare JSON string. Every later
    /// build reads a sentinel an earlier one wrote.
    #[test]
    fn the_sentinel_step_bytes_are_frozen() {
        assert_eq!(GENERATION_SENTINEL, "lash.build.generation");
        let generation = BuildGeneration::from_digest([0x3f, 0xa9, 0x00, 0xbc, 0x12, 0xde]);
        assert_eq!(
            serde_json::to_vec(&generation).expect("encode"),
            b"\"3fa900bc12de\"".to_vec()
        );
    }
}
