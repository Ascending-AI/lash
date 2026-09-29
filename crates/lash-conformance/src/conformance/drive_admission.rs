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
//! with the table switch that fences admissions by the drive epoch (S8/P15), and
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
        admission_bound: usize,
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
                crate::QueuedWorkBatchingConfig::new(1)
                    .with_max_turn_input_admission(admission_bound),
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

    pub(super) async fn runtime(&self) -> crate::LashRuntime {
        self.runtime_over(Arc::clone(&self.store)).await
    }

    /// The law's runtime over `store`: the session's own store, or a law's
    /// decorator of it.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the law's runtime builds"
    )]
    pub(super) async fn runtime_over(
        &self,
        store: Arc<dyn crate::RuntimePersistence>,
    ) -> crate::LashRuntime {
        let state = self.initial_state();
        let policy = state.policy.clone();
        Box::pin(
            crate::LashRuntime::builder(self.host.clone(), crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(crate::testing::test_standard_protocol_factories())
                .with_store(store)
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

/// The two turn-lane heads a root can be admitted on.
#[derive(Clone, Copy, Debug)]
enum HeadKind {
    Input,
    Batch,
}

impl HeadKind {
    const ALL: [Self; 2] = [Self::Input, Self::Batch];

    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Batch => "batch",
        }
    }
}

impl DriveParts {
    /// Enqueue one turn-lane row of `kind` and name it as an admission head.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store accepts the head row"
    )]
    async fn enqueue_head(&self, kind: HeadKind, text: &str) -> crate::store::AdmittedHead {
        match kind {
            HeadKind::Input => crate::store::AdmittedHead::Input(self.enqueue(text, None).await),
            HeadKind::Batch => crate::store::AdmittedHead::Batch(
                self.store
                    .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
                        &self.session_id,
                        text,
                        1,
                        text,
                        crate::DeliveryPolicy::EarliestSafeBoundary,
                    ))
                    .await
                    .expect("enqueue a queued-work head")
                    .batch_id,
            ),
        }
    }

    /// An admission request for `root` headed by `head` under `authority`.
    async fn admit_request(
        &self,
        authority: &crate::store::DriveFence,
        root: &str,
        head: crate::store::AdmittedHead,
        admitted_generation: &'static str,
    ) -> crate::store::AdmitRootRequest {
        crate::store::AdmitRootRequest {
            fence: authority.clone(),
            root: TurnId::from(root),
            head,
            max_inputs: 1,
            policy: crate::testing::queued_work_admission_policy(1),
            base: crate::store::SessionHeadRef {
                generation: 0,
                revision: self.initial_state().head_revision,
                leaf: self
                    .runtime()
                    .await
                    .export_state()
                    .session_graph
                    .leaf_node_id
                    .clone(),
                checkpoint: None,
            },
            turn_index: 1,
            generation: None,
            admitted_generation: crate::engine::BuildGeneration::for_test(admitted_generation),
        }
    }
}

/// FIG-3927 N7: admission delivers. Every row a root's admission or its
/// checkpoint binds has its ingress obligation delivered in the same write,
/// whatever the obligation stood at: due, claimed by a relay's ask, or
/// stalled.
#[expect(
    clippy::expect_used,
    reason = "conformance-law assertions require the store to succeed"
)]
pub async fn admission_delivers_every_row_it_binds(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    use lash_core::store::{
        ObligationKind, ObligationSettlement, ObligationState, StallReason,
        ingress_obligation::ingress_obligation_id,
    };
    let parts = DriveParts::new(prefix, "admission-delivers", &effect_host, &stores, 8).await;
    let ingress = stores.obligation_ledger(ObligationKind::Ingress);
    let now = stores.clock().timestamp_ms();
    let state = |item: String| {
        let ingress = Arc::clone(&ingress);
        async move {
            ingress
                .state(&ingress_obligation_id(&item))
                .await
                .expect("read the obligation")
        }
    };
    let claim = |item: String| {
        let ingress = Arc::clone(&ingress);
        async move {
            ingress
                .claim(&ingress_obligation_id(&item), now, 3_600_000)
                .await
                .expect("claim the obligation")
                .expect("the obligation is due")
        }
    };

    // Next-turn rows in each obligation state, bound by the root admission.
    let due = parts.enqueue("due", None).await;
    let claimed = parts.enqueue("claimed", None).await;
    let stalled = parts.enqueue("stalled", None).await;
    claim(claimed.to_string()).await;
    let stall = claim(stalled.to_string()).await;
    ingress
        .settle(
            &ingress_obligation_id(stalled.as_str()),
            &stall.token,
            ObligationSettlement::Stall {
                reason: StallReason::Refused,
                error: "stalled before its admission".to_string(),
            },
            now,
        )
        .await
        .expect("stall the obligation");
    for (item, expected) in [
        (&due, ObligationState::Due),
        (&claimed, ObligationState::Claimed),
        (&stalled, ObligationState::Stalled),
    ] {
        assert_eq!(state(item.to_string()).await, Some(expected));
    }
    let fence = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
        &parts.store,
        &parts.session_id,
        "admission-delivers",
    )
    .await;
    let root = TurnId::from("admission-delivers-root");
    let admission = lash_core::testing::store_fixtures::admit_root_for_test(
        &parts.store,
        &fence,
        &root,
        crate::store::AdmittedHead::Input(due.clone()),
    )
    .await
    .expect("admit the root")
    .expect("the admission reaches its head");
    assert_eq!(
        admission.input_ids(),
        vec![due.clone(), claimed.clone(), stalled.clone()]
    );
    for item in [&due, &claimed, &stalled] {
        assert_eq!(
            state(item.to_string()).await,
            Some(ObligationState::Delivered),
            "the root admission delivered {item}'s obligation"
        );
    }

    // An active-turn input and a claimed batch, bound by a checkpoint.
    let steer = parts
        .store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            parts.session_id.clone(),
            crate::TurnInputIngress::active_turn(
                &root,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("steer"),
        ))
        .await
        .expect("accept the steering input")
        .input_id;
    let wake = parts
        .store
        .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
            &parts.session_id,
            "wake",
            1,
            "wake",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue the wake")
        .batch_id;
    claim(wake.to_string()).await;
    let checkpoint = lash_core::testing::store_fixtures::admit_at_checkpoint_for_test(
        &parts.store,
        &fence,
        &root,
        &root,
        crate::CheckpointKind::AfterWork,
        "admission-delivers:checkpoint",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await
    .expect("admit at the checkpoint");
    assert_eq!(
        checkpoint
            .inputs
            .as_ref()
            .map(|inputs| inputs.input_ids())
            .unwrap_or_default(),
        vec![steer.clone()]
    );
    assert_eq!(
        checkpoint
            .queued
            .as_ref()
            .map(|queued| queued.batch_ids())
            .unwrap_or_default(),
        vec![wake.clone()]
    );
    for item in [steer.to_string(), wake.to_string()] {
        assert_eq!(
            state(item.clone()).await,
            Some(ObligationState::Delivered),
            "the checkpoint delivered {item}'s obligation"
        );
    }
}

/// N1: a recorded admission owns the session until its root ends. A second
/// root must be refused before it can take any row, whichever turn-lane
/// family either root is headed by.
#[expect(
    clippy::expect_used,
    reason = "conformance-law assertions require the store to succeed"
)]
pub async fn one_unfinished_root_per_session(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for first_kind in HeadKind::ALL {
        for second_kind in HeadKind::ALL {
            let law = format!(
                "one-unfinished-root-{}-{}",
                first_kind.label(),
                second_kind.label()
            );
            let parts = DriveParts::new(prefix, &law, &host, &stores, 1).await;
            let head = parts.enqueue_head(first_kind, "first").await;
            let authority = crate::testing::store_fixtures::seal_drive_fence_for_test(
                &parts.store,
                &parts.session_id,
                "first-root",
            )
            .await;
            let root = TurnId::from("first-root");
            let request = parts
                .admit_request(
                    &authority,
                    "first-root",
                    head.clone(),
                    "one-unfinished-root",
                )
                .await;
            assert!(
                parts
                    .store
                    .admit_root(&request)
                    .await
                    .expect("admit the first root")
                    .is_some(),
                "{law}: the first root reaches its head"
            );
            let unfinished = Some(crate::store::UnfinishedRoot {
                root: root.clone(),
                head,
            });
            assert_eq!(
                parts
                    .store
                    .unfinished_root(&parts.session_id)
                    .await
                    .expect("read the first unfinished root"),
                unfinished,
                "{law}"
            );
            let second_head = parts.enqueue_head(second_kind, "second").await;
            let second = parts
                .admit_request(
                    &authority,
                    "second-root",
                    second_head,
                    "one-unfinished-root",
                )
                .await;
            assert!(
                matches!(
                    parts.store.admit_root(&second).await,
                    Err(crate::StoreError::UnfinishedRootConflict { root: held, .. }) if held == root
                ),
                "{law}: a second root is refused while the first is unfinished"
            );
            assert_eq!(
                parts
                    .store
                    .unfinished_root(&parts.session_id)
                    .await
                    .expect("read the unchanged unfinished root"),
                unfinished,
                "{law}"
            );
        }
    }
}

/// N3 (root half): the store's root record, not the current queue, answers a
/// retry even after more ingress of either family arrives, under the same
/// fence and under a later drive's fence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law assertions require the store and serialization to succeed"
)]
pub async fn a_root_admission_is_idempotent_across_new_rows_and_fences(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for kind in HeadKind::ALL {
        let law = format!("root-admission-idempotent-{}", kind.label());
        let parts = DriveParts::new(prefix, &law, &host, &stores, 1).await;
        let head = parts.enqueue_head(kind, "first").await;
        let first = crate::testing::store_fixtures::seal_drive_fence_for_test(
            &parts.store,
            &parts.session_id,
            "root-admission-first",
        )
        .await;
        let mut request = parts
            .admit_request(&first, "same-root", head, "first-admission")
            .await;
        let recorded = parts
            .store
            .admit_root(&request)
            .await
            .expect("admit the first root")
            .expect("the first root reaches its head");
        let recorded = serde_json::to_value(&recorded).expect("encode first admission");
        for kind in HeadKind::ALL {
            parts
                .enqueue_head(kind, &format!("late-{}", kind.label()))
                .await;
        }
        request.max_inputs = 8;
        request.policy = crate::testing::queued_work_admission_policy(8);
        request.turn_index = 7;
        request.admitted_generation = crate::engine::BuildGeneration::for_test("later-admission");
        let replay = parts
            .store
            .admit_root(&request)
            .await
            .expect("replay admission with later rows")
            .expect("the recorded admission exists");
        assert_eq!(
            recorded,
            serde_json::to_value(&replay).expect("encode replayed admission"),
            "{law}: a same-fence retry answers the recorded admission"
        );
        let later = crate::testing::store_fixtures::seal_drive_fence_for_test(
            &parts.store,
            &parts.session_id,
            "root-admission-later",
        )
        .await;
        request.fence = later.clone();
        let replay = parts
            .store
            .admit_root(&request)
            .await
            .expect("replay admission under a later fence")
            .expect("the recorded admission exists");
        assert_eq!(
            recorded,
            serde_json::to_value(&replay).expect("encode replayed admission"),
            "{law}: a later drive's retry answers the recorded admission"
        );
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
            driver_scope(parts),
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

/// The scope a law's drive runs under on the tier: one per law session, so a
/// later run of the same scope is the tier's recovery of a crashed one.
pub(super) fn driver_scope(parts: &DriveParts) -> crate::AdmittedScope {
    admit(crate::ExecutionScope::turn(
        &parts.session_id,
        TurnId::from("drive-law-driver"),
    ))
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

/// L-S2: one drive admits every item of the admissible prefix under one root:
/// three accepted inputs within the admission bound are answered by one turn.
/// Each acceptance armed its row's ingress obligation, and the root's
/// admission of the row delivered it in the admission's own write (ADR 0109 §3): no ask
/// was ever made, and nothing is owed after.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn one_drive_admits_many_items(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "many-items", &effect_host, &stores, 8).await;
    let first = parts.enqueue("first", Some("many-items-root")).await;
    let second = parts.enqueue("second", None).await;
    let third = parts.enqueue("third", None).await;
    let ingress = stores.obligation_ledger(lash_core::store::ObligationKind::Ingress);
    let obligations = [&first, &second, &third]
        .map(|input| lash_core::store::ingress_obligation::ingress_obligation_id(input.as_str()));
    for id in &obligations {
        assert_eq!(
            ingress.state(id).await.expect("the armed state"),
            Some(lash_core::store::ObligationState::Due),
            "each acceptance armed its row's ingress obligation"
        );
    }
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
        "one root answers every item it admitted"
    );
    assert_eq!(parts.calls.load(Ordering::SeqCst), 1, "one model call");
    for id in &obligations {
        assert_eq!(
            ingress.state(id).await.expect("the settled state"),
            Some(lash_core::store::ObligationState::Delivered),
            "the root's admission of the row delivered its ingress obligation"
        );
    }
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
            after_redrive: None,
            build_generation: None,
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
/// root's admission — carries no drive fence and no epoch, except the seal's
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
                    build_generation: request.build_generation.clone(),
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
            "admit_root",
            crate::RuntimeEffectCommand::AdmitRoot {
                head: crate::store::AdmittedHead::Input(input),
            },
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
/// can read its journal by scope, each root's seal, admission and model call are
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
                keys.contains(&format!("drive-admit:{root}")),
                "root `{root}`'s admission ran under its turn scope: {keys:?}"
            );
            assert!(
                keys.iter().any(|key| key.contains("llm")),
                "root `{root}`'s model call ran under its turn scope: {keys:?}"
            );
        }
        assert!(
            driver_keys.iter().all(|key| !key.starts_with("drive-seal:")
                && !key.starts_with("drive-admit:")
                && !key.contains("llm")),
            "no root effect ran under the driver's scope: {driver_keys:?}"
        );
    }
}

/// Where the root admission faults once.
#[derive(Clone, Copy, Debug)]
enum AdmissionFault {
    /// The admission itself does not answer.
    AtAdmission,
    /// The admission bound its rows, then its answer does not reach the
    /// root: the rows are this attempt's uncommitted-to-journal admission.
    AfterAdmission,
}

/// A session store whose root admission faults once, at [`AdmissionFault`], with a
/// transient contention the next attempt does not meet.
struct AdmissionFaultsOnce {
    inner: Arc<dyn crate::RuntimePersistence>,
    fault: AdmissionFault,
    fired: AtomicUsize,
}

/// A session store whose worker dies once right after the root admission
/// committed, before the effect journal records the admission's outcome
/// (FIG-3840). It keeps every admission it returned, with the drive
/// epoch that asked for it.
struct CrashAfterAdmission {
    inner: Arc<dyn crate::RuntimePersistence>,
    fired: AtomicUsize,
    results: std::sync::Mutex<Vec<(u64, crate::store::RootAdmission)>>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for CrashAfterAdmission {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn admit_root(
        &self,
        request: &crate::store::AdmitRootRequest,
    ) -> Result<Option<crate::store::RootAdmission>, crate::StoreError> {
        let admission = self.inner.admit_root(request).await?;
        if let Some(drive) = &admission {
            self.results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((request.fence.epoch(), drive.clone()));
            if self
                .fired
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                panic!("worker died after the admission commit");
            }
        }
        Ok(admission)
    }
}

/// A root whose worker dies after the store committed its admission, but
/// before the journal recorded the admission's outcome, is redriven by a
/// fresh worker under the same recorded drive admission on exactly the
/// composition, base and executable generation the admission committed
/// (FIG-3840, FIG-3927 N9 (a)). An input that arrives in the window never
/// widens the recorded prefix.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_root_admission_survives_a_worker_crash_without_widening(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts =
        DriveParts::new(prefix, "admission-commit-crash", &effect_host, &stores, 8).await;
    let crash = Arc::new(CrashAfterAdmission {
        inner: Arc::clone(&parts.store),
        fired: AtomicUsize::new(0),
        results: std::sync::Mutex::new(Vec::new()),
    });
    parts.store = Arc::clone(&crash) as Arc<dyn crate::RuntimePersistence>;
    let first = parts.enqueue("first", Some("admission-commit-root")).await;
    let second = parts.enqueue("second", None).await;
    let request = parts.request("admission-commit-drive");
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
                panic!("the admission crash must interrupt the root");
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
                TurnId::from("admission-commit-driver"),
            )),
            crashing,
            redrive,
        )
        .await;
    assert_eq!(
        crash.fired.load(Ordering::SeqCst),
        1,
        "the admission was committed"
    );
    let results = crash
        .results
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let [(crashed_epoch, crashed), (successor_epoch, successor)] = results.as_slice() else {
        panic!("the crashed worker and its successor each admit once: {results:?}");
    };
    assert_eq!(
        crashed_epoch, successor_epoch,
        "the retried admission reuses its sealed drive epoch"
    );
    assert_eq!(
        crashed.input_ids(),
        vec![first.clone(), second.clone()],
        "the crashed worker admitted the prefix queued before it"
    );
    assert_eq!(
        serde_json::to_value(successor).expect("encode the successor's admission"),
        serde_json::to_value(crashed).expect("encode the crashed admission"),
        "the successor drives the recorded composition, base and generation"
    );
    assert_eq!(
        parts.applications().await,
        vec![
            (first, TurnId::from("admission-commit-root")),
            (second, TurnId::from("admission-commit-root"))
        ],
        "the late input must not enter the crashed root's recorded admission"
    );
}

struct NoReplayRepairRead {
    inner: Arc<dyn crate::RuntimePersistence>,
    after_first: AtomicUsize,
    replay_reads: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for NoReplayRepairRead {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        lease: &crate::store::DriveFence,
        binding_id: &str,
        scope: &crate::ExecutionScope,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, crate::StoreError> {
        if self.after_first.load(Ordering::SeqCst) != 0 {
            self.replay_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner
            .pending_turn_cancel_closures(session_id, lease, binding_id, scope)
            .await
    }
}

/// A committed root redriven on its journal replays the admission that included
/// orphan repair. The repair's store read runs only on first execution;
/// the current lease's stop-only head check may still read committed evidence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_root_replays_its_recorded_repair(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts =
        DriveParts::new(prefix, "recorded-head-inspection", &effect_host, &stores, 1).await;
    let read_guard = Arc::new(NoReplayRepairRead {
        inner: Arc::clone(&parts.store),
        after_first: AtomicUsize::new(0),
        replay_reads: AtomicUsize::new(0),
    });
    parts.store = Arc::clone(&read_guard) as Arc<dyn crate::RuntimePersistence>;
    parts
        .enqueue("one answer", Some("recorded-head-root"))
        .await;
    let request = parts.request("recorded-head-drive");
    let first: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let request = request.clone();
        let read_guard = Arc::clone(&read_guard);
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let read_guard = Arc::clone(&read_guard);
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let admitted = admitted(
                    lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                        .await
                        .expect("admit the root"),
                );
                lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted)
                    .await
                    .expect("first root commits");
                read_guard.after_first.store(1, Ordering::SeqCst);
                panic!("worker died after committing the root");
            })
        })
    };
    let redrive: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let admitted = admitted(
                    lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                        .await
                        .expect("replay admission"),
                );
                lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted)
                    .await
                    .expect("redrive uses its recorded admission and repair");
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("recorded-head-driver"),
            )),
            first,
            redrive,
        )
        .await;
    assert_eq!(read_guard.after_first.load(Ordering::SeqCst), 1);
    assert_eq!(read_guard.replay_reads.load(Ordering::SeqCst), 0);
    assert_eq!(parts.calls(), 1, "the model call replays too");
}

impl AdmissionFaultsOnce {
    fn fire(&self, at: AdmissionFault) -> Result<(), crate::StoreError> {
        if std::mem::discriminant(&at) == std::mem::discriminant(&self.fault)
            && self.fired.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(crate::StoreError::Contended);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for AdmissionFaultsOnce {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn admit_root(
        &self,
        request: &crate::store::AdmitRootRequest,
    ) -> Result<Option<crate::store::RootAdmission>, crate::StoreError> {
        self.fire(AdmissionFault::AtAdmission)?;
        let drive = self.inner.admit_root(request).await?;
        self.fire(AdmissionFault::AfterAdmission)?;
        Ok(drive)
    }
}

/// A store that does not answer at a root's admission is that attempt's
/// fault, never the admission's recorded outcome (FIG-3600 review HIGH-3).
/// The root is re-admitted first by every later drive, so a recorded fault
/// would replay under its admission key forever and wedge the session.
/// Instead the attempt aborts open, its retry reads the root's admission
/// back (rows the faulted attempt bound stay bound to the root), and the
/// root commits once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_store_fault_at_the_root_admission_is_retried_not_recorded(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for (fault, law) in [
        (AdmissionFault::AtAdmission, "admission-fault-at-admission"),
        (
            AdmissionFault::AfterAdmission,
            "admission-fault-after-admission",
        ),
    ] {
        let mut parts = DriveParts::new(prefix, law, &effect_host, &stores, 8).await;
        let faults = Arc::new(AdmissionFaultsOnce {
            inner: Arc::clone(&parts.store),
            fault,
            fired: AtomicUsize::new(0),
        });
        parts.store = Arc::clone(&faults) as Arc<dyn crate::RuntimePersistence>;
        let input = parts
            .enqueue("ask once", Some("admission-fault-root"))
            .await;
        let request = parts.request("admission-fault-drive");
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
            vec![(input, TurnId::from("admission-fault-root"))],
            "{fault:?}: the input is applied once"
        );
    }
}

/// ADR 0101 §4 (FIG-3892): the command lane drains first at every turn
/// boundary, and an input root's admission is that boundary. A session
/// command enqueued after the drive admitted the root, before the root's
/// own admission step, never holds the root's head back: the root takes the prefix ahead of the
/// command and commits, while an input enqueued after the command waits.
/// The next drive applies the command before that input runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_command_enqueued_after_an_input_roots_admission_waits_for_the_next_boundary(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "command-after-admission", &effect_host, &stores, 8).await;
    let head = parts
        .enqueue("ahead of the command", Some("command-after-admission-root"))
        .await;
    let request = parts.request("command-after-admission-drive");
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
                    .expect("admit the input root");
                let admitted = admitted(verdict);
                // The command and a later input arrive between the root's
                // drive admission and its own. Both are keyed, so a tier that
                // retries this attempt admits each once.
                parts
                    .store
                    .enqueue_queued_work(
                        crate::QueuedWorkBatchDraft::new(
                            &parts.session_id,
                            crate::DeliveryPolicy::EarliestSafeBoundary,
                            crate::SessionCommand::RefreshToolCatalog {
                                reason: "after the admission".to_string(),
                            },
                        )
                        .with_source_key("command-after-admission-command"),
                    )
                    .await
                    .expect("enqueue the command");
                parts
                    .enqueue("behind the command", Some("command-after-admission-later"))
                    .await;
                match lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted).await {
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
    // The root's first execution must commit: an admission the command held back
    // would fail it retryably, and the tier would retry it forever.
    let driving = tokio::spawn({
        let runner = Arc::clone(&runner);
        let scope = driver_scope(&parts);
        async move { runner.run_turn(scope, attempt).await }
    });
    // A wedged root never ends its first execution: bound the wait so the
    // wedge fails here rather than at the harness's test timeout.
    let first = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
        .await
        .expect("the root's first execution ends instead of retrying its admission forever")
        .expect("the root's first execution ended");
    assert!(
        matches!(
            &first,
            Ok(RootOutcome::Committed { root, .. })
                if root.as_str() == "command-after-admission-root"
        ),
        "the admitted root takes its head past the later command and commits: {first:?}"
    );
    driving.await.expect("the tier settles the drive");
    let root = TurnId::from("command-after-admission-root");
    assert_eq!(
        parts.applications().await,
        vec![(head.clone(), root.clone())],
        "the input behind the command waits for the next boundary"
    );
    let pending = parts
        .store
        .list_open_queued_work(&parts.session_id)
        .await
        .expect("read the command lane");
    assert_eq!(pending.len(), 1, "the command is still open: {pending:?}");

    let next = parts.request("command-after-admission-next");
    let outcome: DriveOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let next = next.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &next)
                .await
                .expect("the next drive runs")
        })
    })
    .await;
    assert_eq!(outcome.stop, DriveStop::Idle, "{outcome:?}");
    assert!(
        matches!(
            outcome.ran.first(),
            Some(RootOutcome::Applied { root })
                if root.as_str().starts_with("drive-commands:")
        ),
        "the next boundary runs the command lane first: {outcome:?}"
    );
    assert!(
        parts
            .store
            .list_open_queued_work(&parts.session_id)
            .await
            .expect("read the command lane")
            .is_empty(),
        "the command applied"
    );
    let applied: Vec<_> = parts
        .applications()
        .await
        .into_iter()
        .map(|(input, _)| input)
        .collect();
    assert_eq!(applied.len(), 2, "both inputs are answered: {applied:?}");
    assert_eq!(applied[0], head, "the head is answered once, first");
}

/// What an idle session's admission takes first: `queued` or `input:<id>`.
async fn idle_admission(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &DriveParts,
    request: &str,
) -> String {
    let request = parts.request(request);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            match lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0).await {
                Ok(AdmitVerdict::Admit(admitted)) => match admitted.work() {
                    lash_core::engine::AdmittedWork::Queued { .. } => "queued".to_owned(),
                    lash_core::engine::AdmittedWork::Commands { .. } => "commands".to_owned(),
                    lash_core::engine::AdmittedWork::Input { head } => format!("input:{head}"),
                    other => format!("{other:?}"),
                },
                other => format!("{other:?}"),
            }
        })
    })
    .await
}

/// ADR 0101 §5, as the FIG-3540 close-out amends it: the turn lane has no
/// kind priority. The two admission tables take one per-session
/// `enqueue_seq`, and an idle session admits whichever of its head host input
/// and its pending queued work came first — a process wake accepted before a
/// host input runs first, an input accepted before a wake runs first — while
/// an open session command still goes ahead of both (§4).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_idle_session_admits_its_turn_lane_in_enqueue_order_whatever_the_kind(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let wake = |parts: &DriveParts, sequence: u64| {
        crate::conformance::helpers::process_wake_work(
            &parts.session_id,
            "idle-order",
            sequence,
            "a wake",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        )
    };

    let wake_first =
        DriveParts::new(prefix, "idle-order-wake-first", &effect_host, &stores, 8).await;
    wake_first
        .store
        .enqueue_queued_work(wake(&wake_first, 1))
        .await
        .expect("queue the earlier wake");
    wake_first
        .enqueue("a later input", Some("idle-order-later-input"))
        .await;
    assert_eq!(
        idle_admission(&runner, &wake_first, "idle-order-wake-first").await,
        "queued",
        "the wake accepted before the input is admitted first"
    );

    let input_first =
        DriveParts::new(prefix, "idle-order-input-first", &effect_host, &stores, 8).await;
    let input = input_first
        .enqueue("an earlier input", Some("idle-order-earlier-input"))
        .await;
    input_first
        .store
        .enqueue_queued_work(wake(&input_first, 1))
        .await
        .expect("queue the later wake");
    assert_eq!(
        idle_admission(&runner, &input_first, "idle-order-input-first").await,
        format!("input:{input}"),
        "the input accepted before the wake is admitted first"
    );

    let command_last =
        DriveParts::new(prefix, "idle-order-command-last", &effect_host, &stores, 8).await;
    command_last
        .enqueue(
            "an input before the command",
            Some("idle-order-command-input"),
        )
        .await;
    command_last
        .store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            command_last.session_id.clone(),
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "a command after the input".to_owned(),
            },
        ))
        .await
        .expect("queue the command");
    assert_eq!(
        idle_admission(&runner, &command_last, "idle-order-command-last").await,
        "commands",
        "the command lane goes ahead of an earlier input"
    );
}

/// One item a turn-lane order law accepts: a next-turn host input, or a
/// process wake under its delivery policy.
#[derive(Clone, Copy, Debug)]
enum LaneItem {
    Input,
    Wake(crate::DeliveryPolicy),
}

/// Accept `items` in order on a fresh law session, drive it to idle, and
/// answer, per item in acceptance order, the model call that first rendered
/// it (`None` when no call did). Queued work drains every compatible row it
/// may (`DrainMode::All`) and one admission takes at most `admission_bound`
/// inputs, so the only thing that keeps an admission from folding later items in is the
/// turn lane's order.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn first_rendering_calls(
    prefix: &str,
    law: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    items: &[LaneItem],
    admission_bound: usize,
) -> Vec<Option<usize>> {
    let mut parts = DriveParts::new(prefix, law, effect_host, stores, admission_bound).await;
    parts.host.durability.queued_work_batching = crate::QueuedWorkBatchingConfig::new(1)
        .with_max_turn_input_admission(admission_bound)
        .with_drain_mode(crate::DrainMode::All);
    let rendered = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let rendered = Arc::clone(&rendered);
            move |request| {
                let messages = serde_json::to_string(&request.messages)
                    .expect("a model request's messages serialize");
                let mut calls = rendered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                calls.push(messages);
                let index = calls.len();
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: format!("answer {index}"),
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

    let marker = |index: usize| format!("{law}-item-{index:02}");
    let mut wake_sequence = 0;
    for (index, item) in items.iter().enumerate() {
        match item {
            LaneItem::Input => {
                parts.enqueue(&marker(index), None).await;
            }
            LaneItem::Wake(policy) => {
                wake_sequence += 1;
                parts
                    .store
                    .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
                        &parts.session_id,
                        law,
                        wake_sequence,
                        &marker(index),
                        *policy,
                    ))
                    .await
                    .expect("accept the law's wake");
            }
        }
    }
    let request = parts.request(&format!("{law}-drive"));
    let outcome: DriveOutcome = on_tier(runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs")
        })
    })
    .await;
    assert_eq!(outcome.stop, DriveStop::Idle, "{law}: {outcome:?}");
    let calls = rendered
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (0..items.len())
        .map(|index| {
            let marker = marker(index);
            calls.iter().position(|call| call.contains(&marker))
        })
        .collect()
}

/// ADR 0101 §5: the turn lane is one FIFO over both admission tables, so a
/// turn never takes an item while an earlier item of the other kind is still
/// unconsumed. What one admission takes — at idle or at a checkpoint — is a
/// contiguous run of the ingress sequence that stops at the first item it
/// cannot deliver, never skipping it. Observed from the model's side: with
/// host inputs and process wakes interleaved, the first call that renders
/// each item never goes backwards in acceptance order, whether the queued
/// run, an input root or a checkpoint admission did the taking.
pub async fn a_turn_never_takes_an_item_past_an_earlier_unconsumed_item_of_the_other_kind(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    use crate::DeliveryPolicy::{AfterCurrentTurnCommit, EarliestSafeBoundary};
    use LaneItem::{Input, Wake};
    let cases: [(&str, Vec<LaneItem>, usize); 3] = [
        // A wake heads the lane: its queued run takes the wake, not the
        // inputs behind it, and never the later wake past an input.
        (
            "lane-wake-first",
            vec![
                Wake(EarliestSafeBoundary),
                Input,
                Wake(EarliestSafeBoundary),
                Input,
            ],
            8,
        ),
        // An input heads the lane: its root takes the input, not the input
        // behind a wake only the next turn can take.
        (
            "lane-input-first",
            vec![Input, Wake(AfterCurrentTurnCommit), Input],
            8,
        ),
        // A checkpoint of the running turn never takes a wake past a
        // next-turn input accepted before it.
        (
            "lane-checkpoint",
            vec![Input, Input, Wake(EarliestSafeBoundary)],
            1,
        ),
    ];
    let mut out_of_order = Vec::new();
    for (law, items, admission_bound) in cases {
        let seen = first_rendering_calls(
            prefix,
            law,
            &effect_host,
            &stores,
            &runner,
            &items,
            admission_bound,
        )
        .await;
        let in_order =
            seen.iter().all(Option::is_some) && seen.windows(2).all(|pair| pair[0] <= pair[1]);
        if !in_order {
            out_of_order.push(format!("{law}: {items:?} first rendered at calls {seen:?}"));
        }
    }
    assert!(
        out_of_order.is_empty(),
        "the model sees every item, in the turn lane's acceptance order:\n{}",
        out_of_order.join("\n")
    );
}
