//! PANICKING-ACTIVATION-PARKS-AT-BUDGET (FIG-6096): an activation that
//! panics before any advance commits is a failed activation, though the
//! runner's hand-back of its actor commits.

use lash_durable::ActorState;
use serde_json::json;

use super::blockers::observed;
use super::{
    LOOP_BUDGET, LawBroken, LawOutcome, SETTLE, SHORT, actor_state, eventually, law_backend,
    noted_calls, payload, root, serve, tag, terminal,
};
use crate::{Backend, ProcessParkReason, ProcessParkState};

/// A process whose every activation panics before any advance commits
/// parks with `ActivationLoop` after exactly the activation-loop budget's
/// claims ran its engine, on the production runner. A process whose
/// panicking claims are interrupted by a committed advance never parks,
/// however many panicked in total, and ends.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_panicking_activation_parks_at_its_budget_and_progress_resets_the_count(
    backend: &Backend,
) -> LawOutcome {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let panicking_tag = tag("panicking");
        let panicking = root(&backend, payload(&panicking_tag, "panic")).await?;
        eventually(SETTLE, "the panicking process parked", || async {
            Ok(actor_state(&backend, &panicking).await? == Some(ActorState::Parked))
        })
        .await?;
        let seen = observed(&backend, &panicking).await?;
        ensure!(
            seen.park
                == ProcessParkState::Parked(ProcessParkReason::ActivationLoop {
                    failed_activations: LOOP_BUDGET,
                }),
            "a panicking process parked {:?}, not at its budget of {LOOP_BUDGET}",
            seen.park
        );
        let ran = noted_calls(&panicking_tag);
        ensure!(
            ran == LOOP_BUDGET,
            "a panicking process ran its engine in {ran} claims, not {LOOP_BUDGET}"
        );

        let flapping_tag = tag("flapping");
        let mut script = payload(&flapping_tag, "flap");
        script["until_ms"] = json!(
            backend
                .durable()
                .now()
                .await?
                .after_millis(i64::try_from(SHORT.as_millis()).unwrap_or(i64::MAX))
                .0
        );
        let flapping = root(&backend, script).await?;
        eventually(SETTLE, "the flapping process ended", || async {
            Ok(terminal(&backend, &flapping).await?.is_some())
        })
        .await?;
        let ran = noted_calls(&flapping_tag);
        ensure!(
            ran == 2 * LOOP_BUDGET,
            "a flapping process ran its engine in {ran} claims, not {}",
            2 * LOOP_BUDGET
        );
        Ok::<_, LawBroken>(())
    }
    .await;
    serving.stop().await;
    result
}
