//! Maintenance outcome-contract conformance (ADR 0067 §4 and §5).
//!
//! One vocabulary, five arms, every backend: a sweep, a completed-but-incomplete
//! pass, a *witnessed* nothing-to-do, a refusal that hands back its partial
//! report, and a failure that is never laundered into a clean empty report.
//!
//! These laws exist because the four backends used to answer four different
//! ways: SQLite absorbed every error into a zero [`GcReport`], the in-memory
//! store returned a zero report unconditionally, `lash-perf` returned a typed
//! error, and Postgres propagated. All four were indistinguishable to a caller
//! reading counters.

use std::sync::Arc;

use super::session_store_factory::session_store_request;
use crate::store::{MaintenanceReport, MaintenanceStop, MaintenanceSweep};

/// Breaks a backend so its next `gc_unreachable` must fail.
///
/// Supplied by backends whose sweep reads durable state that a test can
/// corrupt. A backend with no failure path in its sweep passes `None` to
/// [`store_maintenance_outcome_contract`]; the skip is traced, never silent.
#[async_trait::async_trait]
pub trait StoreMaintenanceFaultInjector: Send + Sync {
    /// Corrupt the store so the next sweep over `session_id` fails.
    async fn break_gc_scope(&self, session_id: &str);
}

/// Every arm of the maintenance outcome contract, on one backend.
///
/// `make` must return a fresh, empty factory. `fault` is the backend's
/// sweep-failure injector, when it has one.
pub async fn store_maintenance_outcome_contract<F>(
    backend: &str,
    make: F,
    fault: Option<Arc<dyn StoreMaintenanceFaultInjector>>,
) where
    F: Fn() -> Arc<dyn crate::SessionStoreFactory>,
{
    report_failure_channels_are_incomplete(backend);
    idle_store_reports_witnessed_nothing_to_do(backend, make()).await;
    superseded_checkpoint_is_a_witnessed_sweep(backend, make()).await;
    empty_root_set_refusal_returns_its_partial_report(backend, make()).await;
    match fault {
        Some(fault) => sweep_failure_is_not_an_empty_report(backend, make(), fault.as_ref()).await,
        None => tracing::warn!(
            backend,
            "backend supplied no gc fault injector: the failure arm of the maintenance \
             outcome contract is unexercised here"
        ),
    }
}

/// A completed report with failed or deferred destructive steps is incomplete,
/// never a healthy empty pass or a clean sweep.
fn report_failure_channels_are_incomplete(backend: &str) {
    let failed_id =
        crate::AttachmentId::parse("maintenance-failed").expect("valid failed attachment id");
    let failed = crate::attachments::AttachmentReclamationReport {
        failed_ids: vec![failed_id],
        ..crate::attachments::AttachmentReclamationReport::default()
    };
    assert_eq!(
        failed.sweep(),
        MaintenanceSweep::Incomplete,
        "{backend}: failed destructive steps must not classify as nothing-to-do: {failed:?}"
    );

    let deferred_id =
        crate::AttachmentId::parse("maintenance-deferred").expect("valid deferred attachment id");
    let deferred = crate::attachments::AttachmentReclamationReport {
        reclaimed_count: 1,
        condemn_deferred_ids: vec![deferred_id],
        ..crate::attachments::AttachmentReclamationReport::default()
    };
    assert_eq!(
        deferred.sweep(),
        MaintenanceSweep::Incomplete,
        "{backend}: deferred destructive steps must not classify as a clean sweep: {deferred:?}"
    );
}

/// A backend that does not implement the levers must say so.
///
/// `lash-perf`'s store is the case this law exists for: an unimplemented lever
/// fails with [`StoreError::UnsupportedStoreOperation`](crate::StoreError::UnsupportedStoreOperation),
/// because reporting an empty sweep it never performed is a lie the counters
/// cannot be distinguished from.
pub async fn store_maintenance_unimplemented_levers_fail(
    backend: &str,
    store: &dyn crate::store::StoreMaintenance,
) {
    let vacuum = store.vacuum().await;
    let vacuum = vacuum.expect_err(&format!(
        "{backend}: an unimplemented vacuum must fail, not report an empty sweep"
    ));
    assert!(
        matches!(
            vacuum.stop,
            MaintenanceStop::Failed(crate::StoreError::UnsupportedStoreOperation { .. })
        ),
        "{backend}: expected an unsupported-operation failure, got {vacuum:?}"
    );
    let gc = store.gc_unreachable().await;
    let gc = gc.expect_err(&format!(
        "{backend}: an unimplemented gc_unreachable must fail, not report an empty sweep"
    ));
    assert!(
        matches!(
            gc.stop,
            MaintenanceStop::Failed(crate::StoreError::UnsupportedStoreOperation { .. })
        ),
        "{backend}: expected an unsupported-operation failure, got {gc:?}"
    );
}

/// An idle session's levers complete and report zero: emptiness that was
/// *observed*, not emptiness standing in for a failure.
async fn idle_store_reports_witnessed_nothing_to_do(
    backend: &str,
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "maintenance-nothing-to-do",
        "maintenance-outcome-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create session store");
    let vacuum = store
        .vacuum()
        .await
        .unwrap_or_else(|error| panic!("{backend}: an idle vacuum must complete: {error:?}"));
    assert_eq!(
        vacuum.sweep(),
        MaintenanceSweep::NothingToDo,
        "{backend}: an idle vacuum reclaims nothing: {vacuum:?}"
    );
    let gc = store
        .gc_unreachable()
        .await
        .unwrap_or_else(|error| panic!("{backend}: an idle sweep must complete: {error:?}"));
    assert_eq!(
        gc.sweep(),
        MaintenanceSweep::NothingToDo,
        "{backend}: an idle sweep reclaims nothing: {gc:?}"
    );
    assert_eq!(
        gc.deleted_blob_count, 0,
        "{backend}: an idle sweep deletes nothing: {gc:?}"
    );
}

/// Superseding a checkpoint orphans its blob, and the next sweep reports the
/// reclaim as a sweep — the arm a backend that swallows errors could never be
/// told apart from.
async fn superseded_checkpoint_is_a_witnessed_sweep(
    backend: &str,
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "maintenance-swept",
        "maintenance-outcome-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create session store");
    let head_revision = commit_generation(&store, &request.session_id, 1, 0).await;
    commit_generation(&store, &request.session_id, 2, head_revision).await;

    let report = store
        .gc_unreachable()
        .await
        .unwrap_or_else(|error| panic!("{backend}: the sweep must complete: {error:?}"));
    assert_eq!(
        report.sweep(),
        MaintenanceSweep::Swept,
        "{backend}: the superseded checkpoint's blob must be reclaimed: {report:?}"
    );
    assert!(
        report.root_count >= 1,
        "{backend}: the live checkpoint is a root: {report:?}"
    );
    let read = store
        .load_session()
        .await
        .expect("load after sweep")
        .expect("session after sweep");
    assert!(
        read.checkpoint.is_some(),
        "{backend}: a sweep must preserve the live checkpoint"
    );
}

/// A refused sweep hands back the report it accumulated before refusing, and
/// destroys nothing.
async fn empty_root_set_refusal_returns_its_partial_report(
    backend: &str,
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "maintenance-refusal",
        "maintenance-outcome-model",
        crate::SessionRelation::Root,
    );
    factory
        .create_store(&request)
        .await
        .expect("create session store");
    let attachments = crate::attachments::InMemoryAttachmentStore::new();
    let orphan = crate::AttachmentStore::put(
        &attachments,
        b"maintenance-refusal-orphan".to_vec(),
        lash_sansio::AttachmentCreateMeta::new(
            lash_sansio::MediaType::parse("application/octet-stream").expect("media type"),
            None,
            Some("orphan".to_string()),
        ),
    )
    .await
    .expect("put a deletion-eligible blob");

    let failure = crate::attachments::reclaim_unreferenced_attachments(
        &*factory,
        &attachments,
        crate::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: crate::EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect_err("an unauthorized empty root set must refuse");

    assert_eq!(
        failure.refusal(),
        Some(&crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized),
        "{backend}: an empty root set refuses rather than failing: {failure:?}"
    );
    assert_eq!(
        failure.partial.scanned_blob_count, 1,
        "{backend}: the refusal must carry the report it had already accumulated: {failure:?}"
    );
    assert_eq!(
        failure.partial.reclaimed_count(),
        0,
        "{backend}: a refusal reclaims nothing: {failure:?}"
    );
    crate::AttachmentStore::get(&attachments, &orphan.id)
        .await
        .expect("the refused sweep left the blob in place");
}

/// A broken sweep fails and says so. This is the law ADR 0067 §4 names: a
/// backend that catches its own error and answers `Ok(GcReport::default())` is
/// indistinguishable from a healthy store with nothing to do, and reds here.
async fn sweep_failure_is_not_an_empty_report(
    backend: &str,
    factory: Arc<dyn crate::SessionStoreFactory>,
    fault: &dyn StoreMaintenanceFaultInjector,
) {
    let request = session_store_request(
        "maintenance-failure",
        "maintenance-outcome-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create session store");
    commit_generation(&store, &request.session_id, 1, 0).await;
    fault.break_gc_scope(&request.session_id).await;

    let failure = match store.gc_unreachable().await {
        Ok(report) => panic!(
            "{backend}: a broken sweep must fail rather than report a clean empty sweep, got \
             Ok({report:?})"
        ),
        Err(failure) => failure,
    };
    assert!(
        matches!(failure.stop, MaintenanceStop::Failed(_)),
        "{backend}: a backend error is a failure, not a refusal: {failure:?}"
    );
    assert_eq!(
        failure.partial.deleted_blob_count, 0,
        "{backend}: a sweep that could not read its roots must claim no deletes: {failure:?}"
    );
}

/// Commit a checkpoint carrying `generation`-specific content, returning the
/// new head revision.
async fn commit_generation(
    store: &Arc<dyn crate::RuntimePersistence>,
    session_id: &str,
    generation: u64,
    expected_head_revision: u64,
) -> u64 {
    let mut state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        head_revision: expected_head_revision,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        crate::ToolState::default().with_generation(generation),
    ));
    let commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
    super::runtime_persistence::commit_runtime_state_for_test(
        store,
        commit,
        &format!("maintenance-outcome-{generation}"),
    )
    .await
    .expect("commit a checkpoint")
    .head_revision
}
