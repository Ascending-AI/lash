//! L-S8 (ADR 0105 §2, O6): a fresh execution of a run that already started
//! is `SubstrateLost`, and runs nothing.
//!
//! A run's execution is its engine execution: a Restate `LashTurn` invocation and
//! its journal. When that journal is gone (purged, or past retention) and the
//! engine runs the same admitted run again, the new execution cannot read
//! what the first one did, so it must not run the run again: a false
//! abandonment is accepted, a duplicate effect is not (FIG-3588). The
//! engine's start marker (a nonce drawn in the run's own journal and set
//! if absent in the store) tells a retry of the first execution from a fresh
//! one.
//!
//! The law runs the same admission twice, each time on a fresh execution the
//! tier admits: the first runs and commits the run; the second must answer
//! `Refused { SubstrateLost }` without asking the model.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{AdmitVerdict, Admitted, RunOutcome, ShiftRequest, ShiftRequestId};
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

#[derive(Clone)]
struct MarkerParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
}

impl MarkerParts {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the law's runtime builds"
    )]
    async fn runtime(&self) -> crate::LashRuntime {
        let policy = crate::testing::mock_session_policy();
        Box::pin(
            crate::LashRuntime::builder(self.host.clone(), crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_plugin_factories(crate::testing::test_standard_protocol_factories())
                .with_store(crate::conformance::helpers::session_view(
                    &self.store,
                    self.session_id.clone(),
                ))
                .with_queued_work(Arc::new(crate::NoSessionWork::new()))
                .build(),
        )
        .await
        .expect("build the run start-marker conformance runtime")
    }
}

/// Run `step` once on a fresh execution the tier admits under `scope`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the tier runs the step once"
)]
async fn fresh_execution<T, F>(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &MarkerParts,
    scope: crate::AdmittedScope,
    step: F,
) -> T
where
    T: Send + 'static,
    F: for<'a> Fn(
            crate::LashRuntime,
            crate::ScopedEffectController<'a>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>
        + Send
        + Sync
        + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let step = Arc::new(step);
    let parts = parts.clone();
    runner
        .run_turn(
            scope,
            Arc::new(move |scoped| {
                let parts = parts.clone();
                let step = Arc::clone(&step);
                let tx = tx.clone();
                Box::pin(async move {
                    let runtime = parts.runtime().await;
                    let value = step(runtime, scoped).await;
                    let _ = tx.send(value);
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    rx.recv().await.expect("the tier ran the law's step")
}

/// Run `admitted`'s run on `scoped`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the run's execution answers"
)]
fn execute_run_on<'a>(
    mut runtime: crate::LashRuntime,
    scoped: crate::ScopedEffectController<'a>,
    admitted: Admitted,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = RunOutcome> + Send + 'a>> {
    Box::pin(async move {
        lash_core::shift::execute_admitted_run(&mut runtime, &scoped, admitted)
            .await
            .expect("the run's execution answers")
    })
}

/// L-S8: a fresh execution of a started run is `SubstrateLost` and runs
/// nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn fresh_execution_of_started_run_is_substrate_lost(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::fixture(format!("{prefix}-run-start-marker"));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: "answered once".into(),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.models = crate::testing::standard_test_llm_profiles(model.into_handle());
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session_id.clone(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("run me once"),
        ))
        .await
        .expect("accept the run's input");
    let parts = MarkerParts {
        session_id: session_id.clone(),
        host,
        store,
    };
    let request = ShiftRequest {
        session: session_id.clone(),
        request: ShiftRequestId::new(format!("{prefix}-run-start-marker")),
        intended_lane: None,
    };

    let admission_scope =
        lash_core::engine::shift_admission_scope(&request.session, &request.request);
    let verdict = fresh_execution(&runner, &parts, admission_scope, {
        let request = request.clone();
        move |mut runtime, scoped| {
            let request = request.clone();
            Box::pin(async move {
                lash_core::shift::admit_shift(&mut runtime, &scoped, &request, 0, None)
                    .await
                    .expect("admission runs")
            })
        }
    })
    .await;
    let admitted: Admitted = match verdict {
        AdmitVerdict::Admit(admitted) => admitted,
        other => panic!("admission admits the pending run: {other:?}"),
    };
    let run = admitted.run().clone();
    let run_scope = |run: &TurnId| lash_core::engine::shift_run_scope(&session_id, run);

    let first = fresh_execution(&runner, &parts, run_scope(&run), {
        let admitted = admitted.clone();
        move |runtime, scoped| execute_run_on(runtime, scoped, admitted.clone())
    })
    .await;
    assert!(
        matches!(first, RunOutcome::Committed { .. }),
        "the run's first execution runs it: {first:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second = fresh_execution(&runner, &parts, run_scope(&run), move |runtime, scoped| {
        execute_run_on(runtime, scoped, admitted.clone())
    })
    .await;
    match second {
        RunOutcome::Refused {
            run: refused,
            refusal: lash_core::engine::SealRefusal::ExecutionLost,
        } => assert_eq!(refused, run),
        other => panic!("a fresh execution of a started run is refused ExecutionLost: {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the fresh execution asks the model nothing"
    );
}

/// Register L-S8 (FIG-3600): a fresh execution of a started run is
/// `SubstrateLost`. The fixture hands back a guard, a prefix, the tier's
/// effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), whose every run
/// is a fresh execution.
#[macro_export]
macro_rules! run_start_marker_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::run_start_marker_tests!(@law [$(#[$attr])*] $fixture;
            (fresh_execution_of_started_run_is_substrate_lost, "run-start-marker"));
    };
    (@law [$(#[$attr:meta])*] $fixture:block; ($law:ident, $label:literal)) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}
