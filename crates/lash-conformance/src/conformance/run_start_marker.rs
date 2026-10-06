//! A fresh root journal cannot reuse an admission whose nonce was drawn in
//! a lost predecessor journal. Atomic admission retains the original fence;
//! the new journal receives ExecutionLost and executes no provider or tool.

use crate::ActorContext;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{AdmitVerdict, Admitted, RunOutcome, ShiftRequest, ShiftRequestId};
use lash_sansio::SessionId;
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
    F: Fn(
            crate::LashRuntime,
            crate::ActorContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>
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
    scoped: crate::ActorContext,
    admitted: Admitted,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = RunOutcome> + Send + 'a>> {
    Box::pin(async move {
        lash_core::shift::execute_admitted_run(&mut runtime, &scoped, admitted)
            .await
            .expect("the run's execution answers")
    })
}

/// L-S8: admission's retained nonce distinguishes a retry from a fresh root
/// journal. The fresh journal refuses before the body and keeps the first fence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_fresh_root_journal_refuses_the_retained_admission_nonce(
    prefix: &str,
    _effect_host: ActorContext,
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
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores)).host_config(
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
    };

    let admission_scope =
        lash_core::engine::shift_admission_scope(&request.session, &request.request);
    let (run, first) = fresh_execution(&runner, &parts, admission_scope.clone(), {
        let request = request.clone();
        move |mut runtime, scoped| {
            let request = request.clone();
            Box::pin(async move {
                let AdmitVerdict::Admit(admitted) =
                    lash_core::shift::admit_shift(&mut runtime, &scoped, &request, 0)
                        .await
                        .expect("the first root admits its input")
                else {
                    panic!("the first root admits pending work");
                };
                assert!(matches!(
                    admitted.root().seal,
                    crate::store::ShiftEpochSeal::Sealed(_)
                ));
                let run = admitted.run().clone();
                let outcome = execute_run_on(runtime, scoped, admitted).await;
                (run, outcome)
            })
        }
    })
    .await;
    assert!(matches!(first, RunOutcome::Committed { .. }), "{first:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let epoch = parts
        .store
        .shift_epoch(&session_id)
        .await
        .expect("retained fence");
    let identity = lash_core::store::AdmissionId::new(format!("{}#0", request.request.as_str()));
    let retained = parts
        .store
        .read_shift_admission(&session_id, &identity)
        .await
        .expect("read atomic admission")
        .expect("first root retained its receipt");

    let second = fresh_execution(
        &runner,
        &parts,
        admission_scope,
        move |mut runtime, scoped| {
            let request = request.clone();
            Box::pin(async move {
                let AdmitVerdict::Admit(admitted) =
                    lash_core::shift::admit_shift(&mut runtime, &scoped, &request, 0)
                        .await
                        .expect("the fresh root reads the retained admission")
                else {
                    panic!("the fresh root returns the nonce refusal with the admission");
                };
                assert!(
                    matches!(
                        admitted.root().seal,
                        crate::store::ShiftEpochSeal::ExecutionLost
                    ),
                    "the fresh root refuses the predecessor's nonce at admission"
                );
                execute_run_on(runtime, scoped, admitted).await
            })
        },
    )
    .await;
    assert!(
        matches!(second, RunOutcome::Refused { run: ref refused,
        refusal: lash_core::engine::SealRefusal::ExecutionLost } if *refused == run),
        "{second:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the lost journal executes nothing again"
    );
    assert_eq!(
        parts
            .store
            .shift_epoch(&session_id)
            .await
            .expect("read unchanged fence"),
        epoch
    );
    let after = parts
        .store
        .read_shift_admission(&session_id, &identity)
        .await
        .expect("read retained admission after refusal")
        .expect("retained receipt");
    assert_eq!(
        after.run_start, retained.run_start,
        "the original nonce is retained"
    );
    assert!(
        matches!(after.seal, crate::store::ShiftEpochSeal::Sealed(_)),
        "the fresh journal never replaces the predecessor's retained authority"
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
            (a_fresh_root_journal_refuses_the_retained_admission_nonce, "run-start-marker"));
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
