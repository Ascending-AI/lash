//! The store half of lost-root recovery: the engine's loss evidence decides
//! whether an open root ends.

use super::{DriveParts, Fixture};
use lash_core::engine::*;
use lash_sansio::TurnId;
use std::sync::Arc;

/// FIG-4281: the engine's evidence that a root's execution is lost decides
/// whether the store ends it. With no run left on the engine, a root that
/// recorded its admission started and ends `SubstrateLost`, settling its
/// input; a root whose record opened but never recorded its admission
/// started nothing and stays open, its input still owed by its ingress
/// obligation. A failed run ends either, since its key never runs again.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_root_with_no_engine_run_ends_only_once_it_started(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let started = Fixture::new(prefix, "lost-started", &host, &stores).await;
    let target = RootRef {
        session: started.parts.session_id.clone(),
        root: started.root.clone(),
    };
    let terminal = started
        .factory
        .end_lost_root(&target, RootRunLoss::NoRun, stores.clock().timestamp_ms())
        .await
        .expect("end the started root")
        .expect("a started root with no engine run ends");
    assert_eq!(
        terminal.cause,
        RootTerminalCause::SubstrateLost { cancelled_by: None }
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
        "the started root's input is settled with it"
    );
    assert!(
        started
            .factory
            .end_lost_root(&target, RootRunLoss::NoRun, stores.clock().timestamp_ms())
            .await
            .expect("a second end")
            .is_none(),
        "a second end writes nothing"
    );

    let parts = DriveParts::new(prefix, "lost-unstarted", &host, &stores, 8).await;
    let root = TurnId::from("lost-unstarted-root");
    let input = parts.enqueue("first", Some(root.as_str())).await;
    let factory = stores.session_store_factory();
    factory
        .bind_root_inputs(&parts.session_id, &root, std::slice::from_ref(&input))
        .await
        .expect("open the root's record without an admission");
    let target = RootRef {
        session: parts.session_id.clone(),
        root: root.clone(),
    };
    assert!(
        factory
            .end_lost_root(&target, RootRunLoss::NoRun, stores.clock().timestamp_ms())
            .await
            .expect("the no-run end of an unstarted root")
            .is_none(),
        "a root that never recorded its admission is left open"
    );
    assert!(
        factory
            .root_terminal(&parts.session_id, &root)
            .await
            .expect("terminal read")
            .is_none(),
        "the unstarted root has no terminal"
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
        "the unstarted root's input is still open for its ingress: {pending:?}"
    );
    let terminal = factory
        .end_lost_root(
            &target,
            RootRunLoss::FailedRun,
            stores.clock().timestamp_ms(),
        )
        .await
        .expect("the failed-run end of an unstarted root")
        .expect("a root whose run failed ends whether or not it started");
    assert_eq!(
        terminal.cause,
        RootTerminalCause::SubstrateLost { cancelled_by: None }
    );
}
