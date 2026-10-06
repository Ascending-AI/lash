//! The store half of lost-run recovery: the engine's loss evidence decides
//! whether an open run ends.

use super::{Fixture, ShiftParts};
use crate::ActorContext;
use lash_core::engine::*;
use lash_sansio::TurnId;
use std::sync::Arc;

/// FIG-4281: the engine's evidence that a run's execution is lost decides
/// whether the store ends it. With no execution left on the engine, a run that
/// recorded its admission started and ends `SubstrateLost`, settling its
/// input; a run whose record opened but never recorded its admission
/// started nothing and stays open, its input still owed by its ingress
/// obligation. A failed run ends either, since its key never runs again.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_run_with_no_engine_execution_ends_only_once_it_started(
    prefix: &str,
    host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let started = Fixture::new(prefix, "lost-started", &host, &stores).await;
    let target = RunRef {
        session: started.parts.session_id.clone(),
        run: started.run.clone(),
    };
    let terminal = started
        .factory
        .end_lost_run(&target, RunLoss::NoRun, stores.clock().timestamp_ms())
        .await
        .expect("end the started run")
        .expect("a started run with no engine execution ends");
    assert_eq!(
        terminal.cause,
        RunTerminalCause::SubstrateLost { cancelled_by: None }
    );
    assert!(
        started
            .parts
            .store
            .list_pending_turn_inputs(&started.parts.session_id)
            .await
            .expect("pending")
            .iter()
            .all(|row| row.input.input_id != started.input),
        "the started run's input is settled with it"
    );
    assert!(
        started
            .factory
            .end_lost_run(&target, RunLoss::NoRun, stores.clock().timestamp_ms())
            .await
            .expect("a second end")
            .is_none(),
        "a second end writes nothing"
    );

    let parts = ShiftParts::new(prefix, "lost-unstarted", &host, &stores, 8).await;
    let run = TurnId::from("lost-unstarted-run");
    let input = parts.enqueue("first", Some(run.as_str())).await;
    let factory = stores.session_store_factory();
    factory
        .bind_run_inputs(&parts.session_id, &run, std::slice::from_ref(&input))
        .await
        .expect("open the run's record without an admission");
    let target = RunRef {
        session: parts.session_id.clone(),
        run: run.clone(),
    };
    assert!(
        factory
            .end_lost_run(&target, RunLoss::NoRun, stores.clock().timestamp_ms())
            .await
            .expect("the no-run end of an unstarted run")
            .is_none(),
        "a run that never recorded its admission is left open"
    );
    assert!(
        factory
            .run_terminal(&parts.session_id, &run)
            .await
            .expect("terminal read")
            .is_none(),
        "the unstarted run has no terminal"
    );
    let pending = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("pending");
    assert!(
        pending.iter().any(|row| row.input.input_id == input
            && !matches!(
                row.status,
                crate::PendingTurnInputReadStatus::Admitted { .. }
            )),
        "the unstarted run's input is still open for its ingress: {pending:?}"
    );
    let terminal = factory
        .end_lost_run(&target, RunLoss::FailedRun, stores.clock().timestamp_ms())
        .await
        .expect("the failed-run end of an unstarted run")
        .expect("a run whose execution failed ends whether or not it started");
    assert_eq!(
        terminal.cause,
        RunTerminalCause::SubstrateLost { cancelled_by: None }
    );
}
