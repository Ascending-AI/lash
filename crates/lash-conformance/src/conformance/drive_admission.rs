//! The session drive's admission laws (FIG-3600, ADR 0105 §2, §11): every
//! root a drive runs is admitted by a recorded `AdmitDrive` step and sealed by
//! a recorded `SealDriveAdmission` step before its first effect.
//!
//! The laws reach an engine only through the kernel's drive entries
//! ([`drive_session`](lash_core::drive::drive_session),
//! [`admit_drive`](lash_core::drive::admit_drive),
//! [`run_admitted_root`](lash_core::drive::run_admitted_root)) on the
//! controller the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner)
//! admits, so every tier that runs turns runs them unchanged.
//!
//! Not here: L-S5 and L-S6 (stale-epoch mutations refused before I/O) land
//! with the table switch that fences claims by the drive epoch (S8/P15), and
//! L-S8 (a fresh execution of a started root is `SubstrateLost`) is the
//! engine's own start marker, so the engine registers it where it keeps one.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{
    AdmitVerdict, Admitted, DriveOutcome, DriveRequest, DriveRequestId, DriveStop, RootOutcome,
    SealVerdict,
};
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;

/// Everything a law's runtime is built from, shared by every run so each is
/// the same session on the same store.
#[derive(Clone)]
pub(super) struct DriveParts {
    pub(super) session_id: SessionId,
    pub(super) host: crate::RuntimeHostConfig,
    pub(super) store: Arc<dyn crate::RuntimePersistence>,
    calls: Arc<AtomicUsize>,
}

impl DriveParts {
    pub(super) async fn new(
        prefix: &str,
        law: &str,
        effect_host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
        claim_bound: usize,
    ) -> Self {
        let session_id = SessionId::from(format!("{prefix}-{law}"));
        let calls = Arc::new(AtomicUsize::new(0));
        let model = crate::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let calls = Arc::clone(&calls);
                move |_request| {
                    let index = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Ok(crate::LlmResponse {
                            parts: vec![crate::LlmOutputPart::Text {
                                text: format!("answer {}", index + 1),
                                response_meta: None,
                            }],
                            ..crate::LlmResponse::default()
                        })
                    }
                }
            })
            .build();
        let mut host = crate::LawBackend::over_stores(Arc::clone(stores), Arc::clone(effect_host))
            .host_config(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1).with_max_turn_input_claim(claim_bound),
            );
        host.providers.provider_resolver =
            Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
        let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
        Self {
            session_id,
            host,
            store,
            calls,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the law's runtime builds"
    )]
    async fn runtime(&self) -> crate::LashRuntime {
        let state = self.initial_state();
        let policy = state.policy.clone();
        Box::pin(
            crate::LashRuntime::builder(self.host.clone(), crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(crate::testing::test_standard_protocol_factories())
                .with_store(Arc::clone(&self.store))
                .with_queued_work(Arc::new(crate::NoSessionWork::new()))
                .build(),
        )
        .await
        .expect("build the drive-admission conformance runtime")
    }

    /// The session state every run of the law's runtime starts from.
    pub(super) fn initial_state(&self) -> crate::RuntimeSessionState {
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            policy,
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        }
    }

    /// How many model calls the law's runs made.
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Accept `text` as next-turn input, keyed by `host_id` when given.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the session store admits the row"
    )]
    pub(super) async fn enqueue(&self, text: &str, host_id: Option<&str>) -> crate::InputId {
        let mut draft = crate::PendingTurnInputDraft::new(
            self.session_id.clone(),
            crate::TurnInputIngress::next_turn(),
            crate::TurnInput::text(text),
        );
        if let Some(host_id) = host_id {
            draft = draft.with_source_key(host_id);
        }
        self.store
            .enqueue_pending_turn_input(draft)
            .await
            .expect("accept the law's input")
            .input_id
    }

    pub(super) fn request(&self, id: &str) -> DriveRequest {
        DriveRequest {
            session: self.session_id.clone(),
            request: DriveRequestId::new(id),
            build_generation: lash_core::engine::BuildGeneration::for_test("conformance-law"),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store reads its own epoch"
    )]
    pub(super) async fn epoch(&self) -> crate::store::StoredDriveEpoch {
        self.store
            .drive_epoch(&self.session_id)
            .await
            .expect("read the session's drive epoch")
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store reads its applications"
    )]
    pub(super) async fn applications(&self) -> Vec<(crate::InputId, TurnId)> {
        self.store
            .list_turn_input_applications(&self.session_id)
            .await
            .expect("read the applications")
            .into_iter()
            .map(|application| (application.input_id, application.turn_id))
            .collect()
    }
}

/// Run `step` once on a controller the tier admits for the law's driver
/// scope, and hand back what it returned.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the tier runs the step once"
)]
pub(super) async fn on_tier<T, F>(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &DriveParts,
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
    let attempt_parts = parts.clone();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("drive-law-driver"),
            )),
            Arc::new(move |scope| {
                let parts = attempt_parts.clone();
                let step = Arc::clone(&step);
                let tx = tx.clone();
                Box::pin(async move {
                    let runtime = parts.runtime().await;
                    let value = step(runtime, scope).await;
                    let _ = tx.send(value);
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    rx.recv().await.expect("the tier ran the law's step")
}

pub(super) fn admitted(verdict: AdmitVerdict) -> Admitted {
    match verdict {
        AdmitVerdict::Admit(admitted) => admitted,
        other => panic!("admission admits the pending root: {other:?}"),
    }
}

/// L-S1: two admissions that observed the same drive epoch are never both
/// authorized. The first seal raises the epoch; the second is superseded and
/// runs nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn one_authorized_drive_per_session(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "one-drive", &effect_host, &stores, 8).await;
    parts
        .enqueue("the only question", Some("one-drive-root"))
        .await;
    let first = parts.request("drive-a");
    let second = parts.request("drive-b");
    let (a, b) = on_tier(&runner, &parts, move |mut runtime, scope| {
        let first = first.clone();
        let second = second.clone();
        Box::pin(async move {
            let a = lash_core::drive::admit_drive(&mut runtime, &scope, &first, 0)
                .await
                .expect("admit the first drive");
            let b = lash_core::drive::admit_drive(&mut runtime, &scope, &second, 0)
                .await
                .expect("admit the second drive");
            let a = lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted(a))
                .await
                .expect("run the first root");
            let b = lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted(b))
                .await
                .expect("the superseded root ends without an abort");
            (a, b)
        })
    })
    .await;
    assert!(
        matches!(&a, RootOutcome::Committed { root, .. } if root.as_str() == "one-drive-root"),
        "{a:?}"
    );
    assert!(
        matches!(
            &b,
            RootOutcome::Refused {
                verdict: SealVerdict::Superseded { epoch: 1 },
                ..
            }
        ),
        "the second admission observed a superseded epoch: {b:?}"
    );
    let epoch = parts.epoch().await;
    assert_eq!(epoch.epoch, 1, "exactly one drive-epoch transition");
    assert_eq!(
        epoch.admission.as_ref().map(|id| id.as_str()),
        Some("drive-a#0")
    );
    assert_eq!(parts.calls.load(Ordering::SeqCst), 1, "one root ran");
}

/// L-S2: one drive claims every item of the claimable prefix under one root:
/// three accepted inputs within the claim bound are answered by one turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn one_drive_claims_many_items(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "many-items", &effect_host, &stores, 8).await;
    let first = parts.enqueue("first", Some("many-items-root")).await;
    let second = parts.enqueue("second", None).await;
    let third = parts.enqueue("third", None).await;
    let request = parts.request("many-items-drive");
    let outcome: DriveOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs")
        })
    })
    .await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(outcome.ran.len(), 1, "one root: {outcome:?}");
    let root = TurnId::from("many-items-root");
    assert_eq!(
        parts.applications().await,
        vec![
            (first, root.clone()),
            (second, root.clone()),
            (third, root.clone())
        ],
        "one root answers every item it claimed"
    );
    assert_eq!(parts.calls.load(Ordering::SeqCst), 1, "one model call");
}

/// L-S3: a redrive of an admitted root drives exactly the claim its first
/// execution recorded: the same answer, no second model call, no second
/// epoch transition, the input applied once. The first execution dies right
/// after its root commits; the tier redelivers it the way it recovers a
/// crashed turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn claim_identity_is_idempotent_within_ownership(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "claim-idempotent", &effect_host, &stores, 8).await;
    let input = parts
        .enqueue("ask once", Some("claim-idempotent-root"))
        .await;
    let request = parts.request("claim-idempotent-drive");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RootOutcome>();
    let attempt = |crash: bool| -> crate::ConformanceTurnAttempt {
        let parts = parts.clone();
        let request = request.clone();
        let tx = tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let admitted = admitted(
                    lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                        .await
                        .expect("admit the root"),
                );
                let outcome = lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted)
                    .await
                    .expect("run the root");
                let _ = tx.send(outcome);
                if crash {
                    panic!("the root's execution dies after its commit");
                }
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("drive-law-driver"),
            )),
            attempt(true),
            attempt(false),
        )
        .await;
    let first = rx.recv().await.expect("the first execution ran its root");
    let again = rx.recv().await.expect("the redrive ran its root");
    assert!(matches!(first, RootOutcome::Committed { .. }), "{first:?}");
    assert_eq!(
        again, first,
        "the redrive drives the same root to the same answer"
    );
    assert_eq!(
        parts.calls.load(Ordering::SeqCst),
        1,
        "no second model call"
    );
    assert_eq!(parts.epoch().await.epoch, 1, "one drive-epoch transition");
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("claim-idempotent-root"))],
        "the input is applied once"
    );
}

/// Crashes a root's execution after its model call and before its commit:
/// the root is admitted and sealed, and nothing it did is committed.
struct CrashBeforeCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PreparedTurn {
            panic!("injected crash after the seal and before the commit");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// L-S4: a redrive of a drive request replays its admissions and seals, so
/// it mints no ownership. The request's drive crashes after its seal and
/// before its commit, and its redrive replays the recorded admission and
/// seal: the epoch transitions exactly once, under the first admission, and
/// the root commits once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn replay_cannot_mint_ownership(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "replay-ownership", &effect_host, &stores, 8).await;
    let input = parts
        .enqueue("ask once", Some("replay-ownership-root"))
        .await;
    let request = parts.request("replay-ownership-drive");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DriveOutcome>();
    let attempt = |crash: bool| -> crate::ConformanceTurnAttempt {
        let parts = parts.clone();
        let request = request.clone();
        let tx = tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                if crash {
                    runtime.set_turn_phase_probe(Arc::new(CrashBeforeCommit));
                }
                let outcome = lash_core::drive::drive_session(&mut runtime, &scope, &request)
                    .await
                    .expect("the redriven drive runs");
                assert!(!crash, "the crash fires before the root commits");
                let _ = tx.send(outcome);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("drive-law-driver"),
            )),
            attempt(true),
            attempt(false),
        )
        .await;
    let outcome = rx.recv().await.expect("the redrive ran the drive");
    assert!(
        matches!(outcome.ran.as_slice(), [RootOutcome::Committed { root, .. }] if root.as_str() == "replay-ownership-root"),
        "the redrive drives the crashed root to its commit: {outcome:?}"
    );
    assert_eq!(outcome.stop, DriveStop::Idle);
    let epoch = parts.epoch().await;
    assert_eq!(epoch.epoch, 1, "the redrive never raises the epoch again");
    assert_eq!(
        epoch.admission.as_ref().map(|id| id.as_str()),
        Some("replay-ownership-drive#0"),
        "the epoch stays sealed by the first admission"
    );
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("replay-ownership-root"))],
        "the root commits once"
    );
}

/// L-S7: admission precedes the first effect: when the root's first model
/// call is made, its admission is already sealed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn admission_precedes_first_effect(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "admission-first", &effect_host, &stores, 8).await;
    let sealed_at_call = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let store = Arc::clone(&parts.store);
            let session_id = parts.session_id.clone();
            let sealed_at_call = Arc::clone(&sealed_at_call);
            move |_request| {
                let store = Arc::clone(&store);
                let session_id = session_id.clone();
                let sealed_at_call = Arc::clone(&sealed_at_call);
                async move {
                    let epoch = store
                        .drive_epoch(&session_id)
                        .await
                        .expect("read the epoch at the model call");
                    sealed_at_call
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(epoch.epoch);
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: "sealed first".to_string(),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    parts.enqueue("ask", Some("admission-first-root")).await;
    let request = parts.request("admission-first-drive");
    let outcome: DriveOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs")
        })
    })
    .await;
    assert_eq!(outcome.ran.len(), 1, "{outcome:?}");
    assert_eq!(
        *sealed_at_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![1],
        "the root's first model call ran under its sealed admission"
    );
}

/// L-S9: an admission whose seal never ran (a reset discarded it) holds
/// nothing: a fresh drive admits and seals anew, and the stale admission's
/// later seal is superseded.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn reset_before_admission_admits_fresh(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "reset-admission", &effect_host, &stores, 8).await;
    parts.enqueue("ask", Some("reset-admission-root")).await;
    let stale = parts.request("reset-admission-stale");
    let fresh = parts.request("reset-admission-fresh");
    let (outcome, late) = on_tier(&runner, &parts, move |mut runtime, scope| {
        let stale = stale.clone();
        let fresh = fresh.clone();
        Box::pin(async move {
            let stale = admitted(
                lash_core::drive::admit_drive(&mut runtime, &scope, &stale, 0)
                    .await
                    .expect("admit the stale drive"),
            );
            let outcome = lash_core::drive::drive_session(&mut runtime, &scope, &fresh)
                .await
                .expect("the fresh drive runs");
            let late = lash_core::drive::run_admitted_root(&mut runtime, &scope, stale)
                .await
                .expect("the stale root ends without an abort");
            (outcome, late)
        })
    })
    .await;
    assert_eq!(outcome.ran.len(), 1, "{outcome:?}");
    assert!(
        matches!(late, RootOutcome::Refused { .. }),
        "a stale admission's late seal is superseded: {late:?}"
    );
    let epoch = parts.epoch().await;
    assert_eq!(epoch.epoch, 1);
    assert_eq!(
        epoch.admission.as_ref().map(|id| id.as_str()),
        Some("reset-admission-fresh#0")
    );
    assert_eq!(parts.calls.load(Ordering::SeqCst), 1);
}

/// L-S10: a parked root blocks admission: while the session's park stands, a
/// fresh drive admits nothing and runs nothing, and the pending work stays
/// accepted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn parked_root_blocks_admission(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "parked-root", &effect_host, &stores, 8).await;
    let input = parts
        .enqueue("waits behind the park", Some("after-the-park"))
        .await;
    parts
        .store
        .record_turn_park(&crate::store::TurnParkWrite {
            session_id: parts.session_id.clone(),
            turn_id: TurnId::from("parked-root"),
            reason: crate::store::ParkReason::ReplayDivergence {
                message: "the drive-admission law parks this root".to_string(),
            },
            at_ms: 1,
            engine: None,
        })
        .await
        .expect("record the park");
    let request = parts.request("parked-root-drive");
    let outcome: DriveOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive stops at the park")
        })
    })
    .await;
    assert!(outcome.ran.is_empty(), "{outcome:?}");
    assert!(
        matches!(&outcome.stop, DriveStop::Parked(park) if park.root.as_str() == "parked-root"),
        "{outcome:?}"
    );
    assert_eq!(parts.epoch().await.epoch, 0, "nothing was sealed");
    assert_eq!(parts.calls.load(Ordering::SeqCst), 0);
    let pending = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("read the pending inputs");
    assert_eq!(
        pending
            .iter()
            .map(|read| read.input.input_id.clone())
            .collect::<Vec<_>>(),
        vec![input],
        "the pending work stays accepted"
    );
}

/// The paths in `value` whose key names a drive epoch or fence.
fn fence_paths(value: &serde_json::Value, path: &str, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let here = format!("{path}.{key}");
                if key.contains("fence") || key.contains("epoch") {
                    found.push(here.clone());
                }
                fence_paths(value, &here, found);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                fence_paths(value, &format!("{path}[{index}]"), found);
            }
        }
        _ => {}
    }
}

/// L-S12: the fence is never part of a recorded envelope. Every command a
/// drive builds from its admission — the admission itself, the seal and the
/// root's claim — carries no drive fence and no epoch, except the seal's
/// `observed_epoch`: the compare-and-set input it decodes from the recorded
/// admission verdict, which every replay reproduces. The fence the seal
/// yields rides its outcome only, and the root's turn effects are built
/// without the admission, so nothing a later drive of the same root issues
/// can differ by epoch.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn fence_is_not_in_the_envelope_hash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "fence-envelope", &effect_host, &stores, 8).await;
    let input = parts.enqueue("ask", Some("fence-envelope-root")).await;
    let request = parts.request("fence-envelope-drive");
    let admit_request = request.clone();
    let admission = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = admit_request.clone();
        Box::pin(async move {
            admitted(
                lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                    .await
                    .expect("admit the root"),
            )
        })
    })
    .await;
    assert_eq!(
        admission.observed_epoch(),
        0,
        "the admission observed the unsealed session"
    );
    let commands = [
        (
            "admit",
            crate::RuntimeEffectCommand::AdmitDrive {
                request: Box::new(lash_core::engine::AdmitRequest {
                    session: request.session.clone(),
                    request: request.request.clone(),
                }),
            },
        ),
        (
            "seal",
            crate::RuntimeEffectCommand::SealDriveAdmission {
                admitted: Box::new(admission),
            },
        ),
        (
            "claim",
            crate::RuntimeEffectCommand::ClaimAcceptedTurnInput { input_id: input },
        ),
    ];
    for (name, command) in commands {
        let value = serde_json::to_value(&command).expect("serialize the drive command");
        let mut found = Vec::new();
        fence_paths(&value, name, &mut found);
        let allowed: &[&str] = if name == "seal" {
            &["seal.admitted.observed_epoch"]
        } else {
            &[]
        };
        let stray: Vec<_> = found
            .iter()
            .filter(|path| !allowed.contains(&path.as_str()))
            .collect();
        assert!(
            stray.is_empty(),
            "the {name} command carries a fence or epoch at {stray:?}: {value}"
        );
    }
}

/// FIG-3607 contract 4: every turn a drive runs is opened by its root: the
/// root is the host's id for the input that starts it, the committed turn
/// answers under that id, and the root's effects ran under the root's own
/// turn scope, never the scope of the caller that drove it. Where the tier
/// can read its journal by scope, each root's seal, claim and model call are
/// recorded under `Turn(root)` and none under the driver's scope.
///
/// Input roots only: a queued root still runs under its `QueueDrain` scope
/// until the queued-run table moves onto the drive (S8), so it is exempt here.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn every_driver_turn_is_owned_by_its_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "owned-root", &effect_host, &stores, 1).await;
    let hosted = parts
        .enqueue("host-named", Some("owned-root-host-id"))
        .await;
    let minted = parts.enqueue("unnamed", None).await;
    let request = parts.request("owned-root-drive");
    let outcome: DriveOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs")
        })
    })
    .await;
    let roots = outcome
        .ran
        .iter()
        .map(|root| root.root().clone())
        .collect::<Vec<_>>();
    assert_eq!(
        roots,
        vec![
            TurnId::from("owned-root-host-id"),
            TurnId::from(minted.as_str())
        ],
        "a root is its input's host id, else its input id"
    );
    assert_eq!(
        parts.applications().await,
        vec![
            (hosted, TurnId::from("owned-root-host-id")),
            (minted.clone(), TurnId::from(minted.as_str()))
        ],
        "each turn answers under its root"
    );
    let driver_scope =
        crate::ExecutionScope::turn(&parts.session_id, TurnId::from("drive-law-driver"));
    if let Some(driver_keys) = runner.recorded_replay_keys(&driver_scope).await {
        for root in &roots {
            let keys = runner
                .recorded_replay_keys(&crate::ExecutionScope::turn(
                    &parts.session_id,
                    root.clone(),
                ))
                .await
                .expect("a tier that reads one scope's journal reads every scope's");
            assert!(
                keys.iter().any(|key| key.starts_with("drive-seal:")),
                "root `{root}`'s seal ran under its turn scope: {keys:?}"
            );
            assert!(
                keys.contains(&format!("drive-claim:{root}")),
                "root `{root}`'s claim ran under its turn scope: {keys:?}"
            );
            assert!(
                keys.iter().any(|key| key.contains("llm")),
                "root `{root}`'s model call ran under its turn scope: {keys:?}"
            );
        }
        assert!(
            driver_keys.iter().all(|key| !key.starts_with("drive-seal:")
                && !key.starts_with("drive-claim:")
                && !key.contains("llm")),
            "no root effect ran under the driver's scope: {driver_keys:?}"
        );
    }
}

/// Where the claim store faults once.
#[derive(Clone, Copy, Debug)]
enum ClaimFault {
    /// The claim itself does not answer.
    AtClaim,
    /// The claim took its rows, then recording the root's admitted base
    /// does not answer: the rows are this attempt's partial claim.
    AfterClaim,
}

/// A session store whose root claim faults once, at [`ClaimFault`], with a
/// transient contention the next attempt does not meet.
struct ClaimFaultsOnce {
    inner: Arc<dyn crate::RuntimePersistence>,
    fault: ClaimFault,
    fired: AtomicUsize,
}

/// A session store whose worker dies once right after the root claim
/// committed, before the effect journal records the claim's outcome
/// (FIG-3840). It keeps every claim result it returned, with the lease
/// generation that asked for it.
struct CrashAfterClaim {
    inner: Arc<dyn crate::RuntimePersistence>,
    fired: AtomicUsize,
    results: std::sync::Mutex<Vec<(u64, crate::AcceptedTurnInputDrive)>>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for CrashAfterClaim {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn claim_root_inputs(
        &self,
        request: &crate::store::RootInputClaimRequest,
    ) -> Result<Option<crate::AcceptedTurnInputDrive>, crate::StoreError> {
        let claim = self.inner.claim_root_inputs(request).await?;
        if let Some(drive) = &claim {
            self.results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((request.lease.fencing_token, drive.clone()));
            if self
                .fired
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                panic!("worker died after the claim commit");
            }
        }
        Ok(claim)
    }
}

/// A root whose worker dies after the store committed its claim, but before
/// the journal recorded the claim's outcome, is redriven by a fresh worker
/// under a new lease generation on exactly the composition, base and
/// executable generation the claim committed (FIG-3840). An input that
/// arrives in the window never widens the recorded prefix.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_claim_commit_survives_a_worker_crash_without_widening(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "claim-commit-crash", &effect_host, &stores, 8).await;
    let crash = Arc::new(CrashAfterClaim {
        inner: Arc::clone(&parts.store),
        fired: AtomicUsize::new(0),
        results: std::sync::Mutex::new(Vec::new()),
    });
    parts.store = Arc::clone(&crash) as Arc<dyn crate::RuntimePersistence>;
    let first = parts.enqueue("first", Some("claim-commit-root")).await;
    let second = parts.enqueue("second", None).await;
    let request = parts.request("claim-commit-drive");
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let request = request.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let admitted = admitted(
                    lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                        .await
                        .expect("admit the root"),
                );
                let _ = lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted).await;
                panic!("the claim crash must interrupt the root");
            })
        })
    };
    let redrive: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            Box::pin(async move {
                let _late = parts.enqueue("late", None).await;
                let mut runtime = parts.runtime().await;
                let admitted = admitted(
                    lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                        .await
                        .expect("readmit the root"),
                );
                lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted)
                    .await
                    .expect("redrive the root");
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("claim-commit-driver"),
            )),
            crashing,
            redrive,
        )
        .await;
    assert_eq!(crash.fired.load(Ordering::SeqCst), 1, "claim was committed");
    let results = crash
        .results
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let [(crashed_lease, crashed), (successor_lease, successor)] = results.as_slice() else {
        panic!("the crashed worker and its successor each claim once: {results:?}");
    };
    assert_ne!(
        crashed_lease, successor_lease,
        "the successor claims under a new lease generation"
    );
    let crate::AcceptedTurnInputDrive::Claimed { claim, .. } = crashed else {
        panic!("the crashed worker claimed its head: {crashed:?}");
    };
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>(),
        vec![first.clone(), second.clone()],
        "the crashed worker claimed the prefix queued before it"
    );
    assert_eq!(
        serde_json::to_value(successor).expect("encode the successor's claim"),
        serde_json::to_value(crashed).expect("encode the crashed claim"),
        "the successor drives the recorded composition, base and generation"
    );
    assert_eq!(
        parts.applications().await,
        vec![
            (first, TurnId::from("claim-commit-root")),
            (second, TurnId::from("claim-commit-root"))
        ],
        "the late input must not enter the crashed root's recorded claim"
    );
}

impl ClaimFaultsOnce {
    fn fire(&self, at: ClaimFault) -> Result<(), crate::StoreError> {
        if std::mem::discriminant(&at) == std::mem::discriminant(&self.fault)
            && self.fired.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(crate::StoreError::Contended);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for ClaimFaultsOnce {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn claim_root_inputs(
        &self,
        request: &crate::store::RootInputClaimRequest,
    ) -> Result<Option<crate::AcceptedTurnInputDrive>, crate::StoreError> {
        self.fire(ClaimFault::AtClaim)?;
        let drive = self.inner.claim_root_inputs(request).await?;
        self.fire(ClaimFault::AfterClaim)?;
        Ok(drive)
    }
}

/// A store that does not answer at a root's claim is that attempt's fault,
/// never the claim's recorded outcome (FIG-3600 review HIGH-3). The root is
/// re-admitted first by every later drive, so a recorded fault would replay
/// under its claim key forever and wedge the session. Instead the attempt
/// aborts open, its retry claims the rows (a partial claim of the faulted
/// attempt is handed back, never left held), and the root commits once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_store_fault_at_the_root_claim_is_retried_not_recorded(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for (fault, law) in [
        (ClaimFault::AtClaim, "claim-fault-at-claim"),
        (ClaimFault::AfterClaim, "claim-fault-after-claim"),
    ] {
        let mut parts = DriveParts::new(prefix, law, &effect_host, &stores, 8).await;
        let faults = Arc::new(ClaimFaultsOnce {
            inner: Arc::clone(&parts.store),
            fault,
            fired: AtomicUsize::new(0),
        });
        parts.store = Arc::clone(&faults) as Arc<dyn crate::RuntimePersistence>;
        let input = parts.enqueue("ask once", Some("claim-fault-root")).await;
        let request = parts.request("claim-fault-drive");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<RootOutcome, String>>();
        let attempt: crate::ConformanceTurnAttempt = {
            let parts = parts.clone();
            Arc::new(move |scope| {
                let parts = parts.clone();
                let request = request.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let mut runtime = parts.runtime().await;
                    let verdict = lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                        .await
                        .expect("admit the root");
                    let lash_core::engine::AdmitVerdict::Admit(admitted) = verdict else {
                        // The engine already retried the faulted execution to
                        // its commit: this run finds nothing to admit.
                        return crate::ConformanceTurnEnd::Settled;
                    };
                    match lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted).await
                    {
                        Ok(outcome) => {
                            let _ = tx.send(Ok(outcome));
                            crate::ConformanceTurnEnd::Settled
                        }
                        Err(abort) => {
                            let error = abort.into_error();
                            let _ = tx.send(Err(error.to_string()));
                            crate::ConformanceTurnEnd::Aborted(error.turn_failure_cause())
                        }
                    }
                })
            })
        };
        let scope = admit(crate::ExecutionScope::turn(
            &parts.session_id,
            TurnId::from("drive-law-driver"),
        ));
        // The faulted execution stays open; the next run of the same scope
        // is its retry, unless the tier's engine already retried it.
        runner.run_turn(scope.clone(), Arc::clone(&attempt)).await;
        runner.run_turn(scope, attempt).await;
        let mut outcomes = Vec::new();
        while let Ok(outcome) = rx.try_recv() {
            outcomes.push(outcome);
        }
        assert_eq!(
            faults.fired.load(Ordering::SeqCst) >= 1,
            true,
            "{fault:?}: the store fault fired"
        );
        let committed: Vec<_> = outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().ok())
            .collect();
        assert!(
            matches!(committed.as_slice(), [RootOutcome::Committed { .. }]),
            "{fault:?}: the retry commits the root exactly once: {outcomes:?}"
        );
        assert_eq!(
            parts.calls.load(Ordering::SeqCst),
            1,
            "{fault:?}: one model call"
        );
        assert_eq!(
            parts.epoch().await.epoch,
            1,
            "{fault:?}: one drive-epoch transition"
        );
        assert_eq!(
            parts.applications().await,
            vec![(input, TurnId::from("claim-fault-root"))],
            "{fault:?}: the input is applied once"
        );
    }
}

/// A drive whose accepted root meets a live foreign lane keeps the input and
/// its invocation open. Its redrive can start while the holder is releasing;
/// once released, the same root answers without a Failed terminal row.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_racing_lane_release_answers_without_a_failed_row(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "lane-release-redrive", &effect_host, &stores, 1).await;
    let root = TurnId::from("lane-release-root");
    let input = parts.enqueue("answer once", Some(root.as_str())).await;
    let request = parts.request("lane-release-drive");
    let holder = crate::LeaseOwnerIdentity::opaque("lane-holder", "lane-holder-incarnation");
    let held = parts
        .store
        .try_claim_session_execution_lease(&parts.session_id, &holder, "foreign-executor", 60_000)
        .await
        .expect("claim the foreign lane")
        .acquired()
        .expect("the lane starts free");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                match lash_core::drive::drive_session(&mut runtime, &scope, &request).await {
                    Ok(outcome) => {
                        let _ = tx.send(Ok(outcome));
                        crate::ConformanceTurnEnd::Settled
                    }
                    Err(abort) => {
                        let error = abort.into_error();
                        let cause = error.turn_failure_cause();
                        let _ = tx.send(Err(error.code));
                        crate::ConformanceTurnEnd::Aborted(cause)
                    }
                }
            })
        })
    };
    let scope = admit(crate::ExecutionScope::turn(
        &parts.session_id,
        TurnId::from("lane-release-driver"),
    ));
    runner.run_turn(scope.clone(), Arc::clone(&attempt)).await;
    assert_eq!(
        rx.try_recv().expect("the busy attempt reports its refusal"),
        Err(crate::RuntimeErrorCode::SessionExecutionLaneBusy)
    );
    assert_eq!(parts.calls(), 0, "a busy lane runs no model call");
    assert!(
        parts
            .store
            .root_terminal(&parts.session_id, &root)
            .await
            .expect("read root terminal")
            .is_none(),
        "a retryable lane refusal writes no Failed row"
    );

    let release_store = Arc::clone(&parts.store);
    let ((), ()) = tokio::join!(
        runner.run_turn(scope.clone(), Arc::clone(&attempt)),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            release_store
                .release_session_execution_lease(&held.completion())
                .await
                .expect("release the foreign lane");
        }
    );
    let redrive = rx.try_recv().expect("the racing redrive reports an answer");
    if redrive == Err(crate::RuntimeErrorCode::SessionExecutionLaneBusy) {
        runner.run_turn(scope, attempt).await;
    } else {
        assert!(
            redrive.is_ok(),
            "the racing redrive only waits for the lane: {redrive:?}"
        );
    }
    let final_answer = if redrive.is_ok() {
        redrive
    } else {
        rx.try_recv()
            .expect("the released lane lets the redrive finish")
    };
    assert!(
        matches!(&final_answer, Ok(DriveOutcome { ran, .. }) if matches!(ran.as_slice(), [RootOutcome::Committed { .. }])),
        "the root commits once after release: {final_answer:?}"
    );
    assert_eq!(parts.calls(), 1, "the root makes one model call");
    assert_eq!(parts.applications().await, vec![(input, root.clone())]);
    assert_eq!(
        parts
            .store
            .root_terminal(&parts.session_id, &root)
            .await
            .expect("read terminal after redrive")
            .map(|row| row.kind),
        Some(crate::store::RootTerminalKind::Answered),
        "the root has an Answered row and no Failed row"
    );
}
