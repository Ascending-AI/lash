//! CANCEL-CLAIM-PARKS-WITHOUT-STRANDING, the process's half (FIG-6099): a
//! claim only for a cancel is bounded by the activation-loop budget like
//! any other, and its park keeps the cancel it exists for.

use std::sync::atomic::AtomicU32;

use lash_durable::CommitLabel;

use super::blockers::observed;
use super::*;
use crate::testing::DurableCommitHook;
use crate::{ProcessParkReason, ProcessParkState};

/// A process claimed only for its cancel (by a node whose engine does not
/// decode its state) whose engine-free end crashes on every claim parks
/// `CancelLoop` after exactly the activation-loop budget's claims, instead
/// of being claimed again without bound. Its cancel stays recorded and
/// unended, and an operator's redrive ends it `Cancelled` with no engine
/// code run.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_cancel_that_fails_every_claim_parks_cancel_loop_and_a_redrive_runs_it(
    backend: &Backend,
) -> LawOutcome {
    let backend = law_backend(backend)?;
    let process_tag = tag("cancel-loop");
    let process = root(&backend, payload(&process_tag, "hold")).await?;
    let serving = serve(&backend);
    let started = eventually(SETTLE, "the process started and waits", || {
        settled_waiting(&backend, &process)
    })
    .await;
    serving.stop().await;
    started?;
    // The next node's engine writes a state format the stored state is not
    // in: it claims the process only for its cancel, and every end it
    // commits crashes.
    let newer = with_engines(&backend, vec![Arc::new(LawEngine { version: 1 })])?;
    let key = actor(&process)?;
    let crashed = Arc::new(AtomicU32::new(0));
    let hooked = Arc::new(DurableCommitHook::over(Arc::clone(newer.durable())));
    hooked.arm({
        let crashed = Arc::clone(&crashed);
        move |tx, label| {
            if label == CommitLabel::PROCESS_TERMINAL && tx.actor() == &key {
                crashed.fetch_add(1, Ordering::SeqCst);
                panic!("cancel-loop: the engine-free end crashes");
            }
            std::future::ready(Ok(()))
        }
    });
    let advanced = advances(&process_tag);
    let serving = serve_over(&newer, NODE, Vec::new(), Arc::clone(&hooked) as _);
    let result = async {
        cancel(&newer, &process).await?;
        eventually(SETTLE, "the cancelled process parked", || async {
            Ok(actor_state(&newer, &process).await? == Some(ActorState::Parked))
        })
        .await?;
        let seen = observed(&newer, &process).await?;
        ensure!(
            seen.park
                == ProcessParkState::Parked(ProcessParkReason::CancelLoop {
                    failed_activations: LOOP_BUDGET,
                }),
            "the process parked {:?}, not CancelLoop at its budget of {LOOP_BUDGET}",
            seen.park
        );
        let ends = crashed.load(Ordering::SeqCst);
        ensure!(
            ends == LOOP_BUDGET,
            "the process's end crashed in {ends} claims, not {LOOP_BUDGET}"
        );
        ensure!(
            terminal(&newer, &process).await?.is_none()
                && record(&newer, &process).await?.cancel_request.is_some(),
            "the process parked without its cancel recorded and pending"
        );
        hooked.disarm();
        ensure!(
            newer.redrive_process(&process, "process-laws").await?,
            "the redrive found the process not parked"
        );
        eventually(SETTLE, "the redriven process ended", || async {
            Ok(terminal(&newer, &process).await?.is_some())
        })
        .await?;
        let end = terminal(&newer, &process).await?.unwrap_or_default();
        ensure!(
            cancellation(&end) == Some(("operator_requested".to_owned(), false)),
            "the process did not end cancelled: {end}"
        );
        ensure!(
            advances(&process_tag) == advanced,
            "the process ran engine code for its cancel: {:?}",
            advances(&process_tag)
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}
