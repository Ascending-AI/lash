//! The session shift's admission laws (FIG-3600, ADR 0105 §2, §11): every
//! run a shift runs is admitted by a recorded `AdmitShift` step and sealed by
//! a recorded `SealShiftAdmission` step before its first effect.
//!
//! The laws reach an engine only through the kernel's shift entries
//! ([`work_session`](lash_core::shift::work_session),
//! [`admit_shift`](lash_core::shift::admit_shift),
//! [`execute_admitted_run`](lash_core::shift::execute_admitted_run)) on the
//! controller the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner)
//! admits, so every tier that runs turns runs them unchanged.
//!
//! Not here: L-S5 and L-S6 (stale-epoch mutations refused before I/O) land
//! with the table switch that fences admissions by the shift epoch (S8/P15), and
//! L-S8 (a fresh execution of a started run is `SubstrateLost`) is the
//! engine's own start marker, so the engine registers it where it keeps one.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{
    AdmitVerdict, Admitted, RunOutcome, SealRefusal, ShiftOutcome, ShiftRequest, ShiftRequestId,
    ShiftStop,
};
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;

/// Everything a law's runtime is built from, shared by every run so each is
/// the same session on the same store.
#[derive(Clone)]
pub(super) struct ShiftParts {
    pub(super) session_id: SessionId,
    pub(super) host: crate::RuntimeHostConfig,
    pub(super) store: Arc<dyn crate::RuntimeStore>,
    /// The protocol session the law's runtime runs, in place of the
    /// standard test protocol's.
    pub(super) protocol: Option<Arc<dyn lash_core::plugin::ProtocolSessionPlugin>>,
    /// A law's explicit creator head, cloned unchanged when the runtime reopens.
    pub(super) initial_head: Option<crate::RuntimeSessionState>,
    /// Plugins a law's runtime installs beside the protocol's.
    pub(super) plugins: Vec<Arc<dyn crate::plugin::PluginFactory>>,
    calls: Arc<AtomicUsize>,
}

impl ShiftParts {
    pub(super) async fn new(
        prefix: &str,
        law: &str,
        effect_host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
        admission_bound: usize,
    ) -> Self {
        let session_id = SessionId::fixture(format!("{prefix}-{law}"));
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
        host.providers.models = crate::testing::standard_test_llm_profiles(model.into_handle());
        let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
        Self {
            session_id,
            host,
            store,
            protocol: None,
            initial_head: None,
            plugins: Vec::new(),
            calls,
        }
    }

    /// The law's host takes every eligible next-turn input, up to the
    /// admission bound, into one run (`DrainMode::All`): the law is about a
    /// composed run, which the default drain never forms (FIG-4457).
    pub(super) fn compose_inputs(&mut self) {
        self.host.durability.queued_work_batching = self
            .host
            .durability
            .queued_work_batching
            .clone()
            .with_drain_mode(crate::DrainMode::All);
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
        store: Arc<dyn crate::RuntimeStore>,
    ) -> crate::LashRuntime {
        let state = self.initial_state();
        let policy = state.policy.clone();
        Box::pin(
            crate::LashRuntime::builder(self.host.clone(), crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(
                    match &self.protocol {
                        Some(protocol) => {
                            vec![
                                crate::testing::test_standard_protocol_factory_with_runtime_state(
                                    Arc::clone(protocol),
                                    None,
                                ),
                            ]
                        }
                        None => crate::testing::test_standard_protocol_factories(),
                    }
                    .into_iter()
                    .chain(self.plugins.iter().cloned())
                    .collect(),
                )
                .with_store(crate::conformance::helpers::session_view(
                    &store,
                    self.session_id.clone(),
                ))
                .with_queued_work(Arc::new(crate::NoSessionWork::new()))
                .build(),
        )
        .await
        .expect("build the shift-admission conformance runtime")
    }

    /// The session state every run of the law's runtime starts from.
    pub(super) fn initial_state(&self) -> crate::RuntimeSessionState {
        if let Some(state) = &self.initial_head {
            return state.clone();
        }
        let policy = crate::testing::mock_session_policy();
        crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            policy,
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
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

    pub(super) fn request(&self, id: &str) -> ShiftRequest {
        ShiftRequest {
            session: self.session_id.clone(),
            request: ShiftRequestId::new(id),
            intended_lane: None,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store reads its own epoch"
    )]
    pub(super) async fn epoch(&self) -> crate::store::StoredShiftEpoch {
        self.store
            .shift_epoch(&self.session_id)
            .await
            .expect("read the session's shift epoch")
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

/// The two turn-lane heads a run can be admitted on.
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

impl ShiftParts {
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

    /// An admission request for `run` headed by `head` under `authority`.
    async fn admit_request(
        &self,
        authority: &crate::store::ShiftFence,
        run: &str,
        head: crate::store::AdmittedHead,
        admitted_generation: &'static str,
    ) -> crate::store::AdmitRunRequest {
        crate::store::AdmitRunRequest {
            fence: authority.clone(),
            run: TurnId::fixture(run),
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
            executor: crate::store::RunExecutor::run(&crate::store::AdmissionId::new("fixture#0")),
            plugins: Default::default(),
            turn_cancellation: None,
            trace_scopes: std::sync::Arc::new(lash_core::UntracedScopes),
        }
    }
}

/// FIG-4848 G3: binding the selected rows also selects their cancellation
/// authority. Adoption validates that authority even when the rows were
/// already admitted. Both turn-lane head kinds obey the same transaction.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub async fn run_admission_binds_cancellation_authority_with_its_rows(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for kind in HeadKind::ALL {
        let parts = ShiftParts::new(prefix, kind.label(), &effect_host, &stores, 1).await;
        let head = parts.enqueue_head(kind, "admitted with authority").await;
        let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
            &parts.store,
            &parts.session_id,
            "bound-authority",
        )
        .await;
        let mut request = parts
            .admit_request(&fence, "bound-run", head, "binding-law")
            .await;
        let scope = crate::ExecutionScope::turn(&parts.session_id, &request.run);
        request.turn_cancellation = Some(crate::store::TurnCancellationBinding {
            binding_id: "selected-authority".into(),
            admitted_scope: scope.clone(),
        });
        let admitted = parts
            .store
            .admit_run(&request)
            .await
            .expect("admit with authority")
            .expect("head selected");
        let adopted = parts
            .store
            .admit_run(&request)
            .await
            .expect("adopt with the same authority")
            .expect("admission retained");
        assert_eq!(adopted.input_ids(), admitted.input_ids());
        assert_eq!(adopted.batch_ids(), admitted.batch_ids());
        assert!(
            matches!(parts.store.validate_turn_cancellation_binding(
                &parts.session_id, &fence, "other-authority", &scope,
            ).await, Err(crate::StoreError::TurnCancelBindingMismatch { expected, presented, .. })
                if expected == "selected-authority" && presented == "other-authority"
            ),
            "the admission itself selected the authority"
        );
        request
            .turn_cancellation
            .as_mut()
            .expect("binding")
            .binding_id = "other-authority".into();
        assert!(
            matches!(parts.store.admit_run(&request).await,
                Err(crate::StoreError::TurnCancelBindingMismatch { expected, presented, .. })
                    if expected == "selected-authority" && presented == "other-authority"
            ),
            "adoption cannot bypass the selected authority"
        );
    }
}

/// FIG-4848 G3 and the prepared trace composition rule: preparation selects
/// no authority, and a commit that misses its exact proposal binds nothing.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub async fn preparing_or_refusing_admission_leaves_cancellation_authority_unbound(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for cancel_head in [false, true] {
        let law = if cancel_head {
            "stale-proposal"
        } else {
            "unbound-proposal"
        };
        let parts = ShiftParts::new(prefix, law, &effect_host, &stores, 1).await;
        let input = parts.enqueue("proposed row", None).await;
        let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
            &parts.store,
            &parts.session_id,
            law,
        )
        .await;
        let mut request = parts
            .admit_request(
                &fence,
                "proposed-run",
                crate::store::AdmittedHead::Input(input.clone()),
                "binding-law",
            )
            .await;
        let scope = crate::ExecutionScope::turn(&parts.session_id, &request.run);
        request.turn_cancellation = Some(crate::store::TurnCancellationBinding {
            binding_id: "proposed-authority".into(),
            admitted_scope: scope.clone(),
        });
        let prepared = parts
            .store
            .prepare_run_admission(&request)
            .await
            .expect("prepare")
            .expect("proposal selects the head");
        if cancel_head {
            parts
                .store
                .cancel_pending_turn_input(&parts.session_id, &input)
                .await
                .expect("withdraw proposed head");
            assert!(
                matches!(
                    parts
                        .store
                        .commit_run_admission(&prepared, &lash_core::TraceAnchor::Untraced)
                        .await,
                    Err(crate::StoreError::PreparedRunAdmissionStale { .. })
                ),
                "exact proposal revalidation refuses the withdrawn head"
            );
        }
        parts
            .store
            .validate_turn_cancellation_binding(
                &parts.session_id,
                &fence,
                "other-authority",
                &scope,
            )
            .await
            .expect("preparation and refused commits select no authority");
        if !cancel_head {
            assert!(
                matches!(
                    parts
                        .store
                        .commit_run_admission(&prepared, &lash_core::TraceAnchor::Untraced)
                        .await,
                    Err(crate::StoreError::TurnCancelBindingMismatch { .. })
                ),
                "commit revalidates the authority after trace preparation"
            );
            let pending = parts
                .store
                .pending_turn_input(&parts.session_id, &input)
                .await
                .expect("pending row")
                .expect("row retained");
            assert_eq!(pending.status, crate::PendingTurnInputReadStatus::Open);
        }
    }
}

/// FIG-3927 N7: admission delivers. Every row a run's admission or its
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
    use lash_core::store::{ObligationKind, ObligationSettlement, ObligationState, StallReason};
    let parts = ShiftParts::new(prefix, "admission-delivers", &effect_host, &stores, 8).await;
    let ingress = stores.obligation_ledger(ObligationKind::Ingress);
    let now = stores.clock().timestamp_ms();
    let state = |item: String| {
        let ingress = Arc::clone(&ingress);
        let session_id = parts.session_id.clone();
        async move {
            ingress
                .state(
                    &lash_core::store::ObligationKey::Ingress {
                        session_id: session_id.clone(),
                        item_id: item.to_string(),
                    }
                    .id(),
                )
                .await
                .expect("read the obligation")
        }
    };
    let claim = |item: String| {
        let ingress = Arc::clone(&ingress);
        let session_id = parts.session_id.clone();
        async move {
            ingress
                .claim(
                    &lash_core::store::ObligationKey::Ingress {
                        session_id: session_id.clone(),
                        item_id: item.to_string(),
                    }
                    .id(),
                    &crate::store::ClaimToken::mint(),
                    now,
                    3_600_000,
                )
                .await
                .expect("claim the obligation")
                .expect("the obligation is due")
        }
    };

    // Next-turn rows in each obligation state, bound by the run admission.
    let due = parts.enqueue("due", None).await;
    let claimed = parts.enqueue("claimed", None).await;
    let stalled = parts.enqueue("stalled", None).await;
    claim(claimed.to_string()).await;
    let stall = claim(stalled.to_string()).await;
    ingress
        .settle(
            &lash_core::store::ObligationKey::Ingress {
                session_id: parts.session_id.clone(),
                item_id: (stalled.as_str()).to_string(),
            }
            .id(),
            &stall.token,
            ObligationSettlement::Stall {
                reason: StallReason::Refused,
                error: crate::store::DeliveryError::new(
                    crate::RuntimeErrorCode::EngineControlRequest,
                    "stalled before its admission",
                ),
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
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        &parts.store,
        &parts.session_id,
        "admission-delivers",
    )
    .await;
    let run = TurnId::from("admission-delivers-run");
    let admission = lash_core::testing::store_fixtures::admit_run_for_test(
        &parts.store,
        &fence,
        &run,
        crate::store::AdmittedHead::Input(due.clone()),
    )
    .await
    .expect("admit the run")
    .expect("the admission reaches its head");
    assert_eq!(
        admission.input_ids(),
        vec![due.clone(), claimed.clone(), stalled.clone()]
    );
    for item in [&due, &claimed, &stalled] {
        assert_eq!(
            state(item.to_string()).await,
            Some(ObligationState::Delivered),
            "the run admission delivered {item}'s obligation"
        );
    }

    // An active-turn input and a claimed batch, bound by a checkpoint.
    let steer = parts
        .store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            parts.session_id.clone(),
            crate::TurnInputIngress::active_turn(
                &run,
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
        &run,
        &run,
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

/// N1: a recorded admission owns the session until its run ends. A second
/// run must be refused before it can take any row, whichever turn-lane
/// family either run is headed by.
#[expect(
    clippy::expect_used,
    reason = "conformance-law assertions require the store to succeed"
)]
pub async fn one_unfinished_run_per_session(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for first_kind in HeadKind::ALL {
        for second_kind in HeadKind::ALL {
            let law = format!(
                "one-unfinished-run-{}-{}",
                first_kind.label(),
                second_kind.label()
            );
            let parts = ShiftParts::new(prefix, &law, &host, &stores, 1).await;
            let head = parts.enqueue_head(first_kind, "first").await;
            let authority = crate::testing::store_fixtures::seal_shift_fence_for_test(
                &parts.store,
                &parts.session_id,
                "first-run",
            )
            .await;
            let run = TurnId::from("first-run");
            let request = parts
                .admit_request(&authority, "first-run", head.clone(), "one-unfinished-run")
                .await;
            assert!(
                parts
                    .store
                    .admit_run(&request)
                    .await
                    .expect("admit the first run")
                    .is_some(),
                "{law}: the first run reaches its head"
            );
            let unfinished = Some(crate::store::UnfinishedRun {
                run: run.clone(),
                head,
                executor: request.executor.clone(),
            });
            assert_eq!(
                parts
                    .store
                    .unfinished_run(&parts.session_id)
                    .await
                    .expect("read the first unfinished run"),
                unfinished,
                "{law}"
            );
            let second_head = parts.enqueue_head(second_kind, "second").await;
            let second = parts
                .admit_request(&authority, "second-run", second_head, "one-unfinished-run")
                .await;
            assert!(
                matches!(
                    parts.store.admit_run(&second).await,
                    Err(crate::StoreError::UnfinishedRunConflict { run: held, .. }) if held == run
                ),
                "{law}: a second run is refused while the first is unfinished"
            );
            assert_eq!(
                parts
                    .store
                    .unfinished_run(&parts.session_id)
                    .await
                    .expect("read the unchanged unfinished run"),
                unfinished,
                "{law}"
            );
        }
    }
}

/// N3 (run half): the store's run record, not the current queue, answers a
/// retry even after more ingress of either family arrives, under the same
/// fence and under a later shift's fence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law assertions require the store and serialization to succeed"
)]
pub async fn a_run_admission_is_idempotent_across_new_rows_and_fences(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for kind in HeadKind::ALL {
        let law = format!("run-admission-idempotent-{}", kind.label());
        let parts = ShiftParts::new(prefix, &law, &host, &stores, 1).await;
        let head = parts.enqueue_head(kind, "first").await;
        let first = crate::testing::store_fixtures::seal_shift_fence_for_test(
            &parts.store,
            &parts.session_id,
            "run-admission-first",
        )
        .await;
        let mut request = parts
            .admit_request(&first, "same-run", head, "first-admission")
            .await;
        let recorded = parts
            .store
            .admit_run(&request)
            .await
            .expect("admit the first run")
            .expect("the first run reaches its head");
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
            .admit_run(&request)
            .await
            .expect("replay admission with later rows")
            .expect("the recorded admission exists");
        assert_eq!(
            recorded,
            serde_json::to_value(&replay).expect("encode replayed admission"),
            "{law}: a same-fence retry answers the recorded admission"
        );
        let later = crate::testing::store_fixtures::seal_shift_fence_for_test(
            &parts.store,
            &parts.session_id,
            "run-admission-later",
        )
        .await;
        request.fence = later.clone();
        let replay = parts
            .store
            .admit_run(&request)
            .await
            .expect("replay admission under a later fence")
            .expect("the recorded admission exists");
        assert_eq!(
            recorded,
            serde_json::to_value(&replay).expect("encode replayed admission"),
            "{law}: a later shift's retry answers the recorded admission"
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
    parts: &ShiftParts,
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

/// The scope a law's shift runs under on the tier: one per law session, so a
/// later run of the same scope is the tier's recovery of a crashed one.
pub(super) fn driver_scope(parts: &ShiftParts) -> crate::AdmittedScope {
    admit(crate::ExecutionScope::turn(
        &parts.session_id,
        TurnId::from("shift-law-driver"),
    ))
}

pub(super) fn admitted(verdict: AdmitVerdict) -> Admitted {
    match verdict {
        AdmitVerdict::Admit(admitted) => admitted,
        other => panic!("admission admits the pending run: {other:?}"),
    }
}

/// L-S1: two admissions that observed the same shift epoch are never both
/// authorized. The first seal raises the epoch; the second is superseded and
/// runs nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn one_authorized_shift_per_session(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "one-shift", &effect_host, &stores, 8).await;
    parts
        .enqueue("the only question", Some("one-shift-run"))
        .await;
    let first = parts.request("shift-a");
    let second = parts.request("shift-b");
    let (a, b) = on_tier(&runner, &parts, move |mut runtime, scope| {
        let first = first.clone();
        let second = second.clone();
        Box::pin(async move {
            let a = lash_core::shift::admit_shift(&mut runtime, &scope, &first, 0, None)
                .await
                .expect("admit the first shift");
            let b = lash_core::shift::admit_shift(&mut runtime, &scope, &second, 0, None)
                .await
                .expect("admit the second shift");
            let a = lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted(a))
                .await
                .expect("run the first run");
            let b = lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted(b))
                .await
                .expect("the superseded run ends without an abort");
            (a, b)
        })
    })
    .await;
    assert!(
        matches!(&a, RunOutcome::Committed { run, .. } if run.as_str() == "one-shift-run"),
        "{a:?}"
    );
    assert!(
        matches!(
            &b,
            RunOutcome::Refused {
                refusal: SealRefusal::Superseded { epoch: 1 },
                ..
            }
        ),
        "the second admission observed a superseded epoch: {b:?}"
    );
    let epoch = parts.epoch().await;
    assert_eq!(epoch.epoch, 1, "exactly one shift-epoch transition");
    assert_eq!(epoch.admission().map(|id| id.as_str()), Some("shift-a#0"));
    assert_eq!(parts.calls.load(Ordering::SeqCst), 1, "one run ran");
}

/// L-S2: one shift admits every item of the admissible prefix under one run:
/// three accepted inputs within the admission bound are answered by one turn.
/// Each acceptance armed its row's ingress obligation, and the run's
/// admission of the row delivered it in the admission's own write (ADR 0109 §3): no ask
/// was ever made, and nothing is owed after.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn one_shift_admits_many_items(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "many-items", &effect_host, &stores, 8).await;
    parts.compose_inputs();
    let first = parts.enqueue("first", Some("many-items-run")).await;
    let second = parts.enqueue("second", None).await;
    let third = parts.enqueue("third", None).await;
    let ingress = stores.obligation_ledger(lash_core::store::ObligationKind::Ingress);
    let obligations = [&first, &second, &third].map(|input| {
        lash_core::store::ObligationKey::Ingress {
            session_id: parts.session_id.clone(),
            item_id: (input.as_str()).to_string(),
        }
        .id()
    });
    for id in &obligations {
        assert_eq!(
            ingress.state(id).await.expect("the armed state"),
            Some(lash_core::store::ObligationState::Due),
            "each acceptance armed its row's ingress obligation"
        );
    }
    let request = parts.request("many-items-shift");
    let outcome: ShiftOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs")
        })
    })
    .await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(outcome.ran.len(), 1, "one run: {outcome:?}");
    let run = TurnId::from("many-items-run");
    assert_eq!(
        parts.applications().await,
        vec![
            (first, run.clone()),
            (second, run.clone()),
            (third, run.clone())
        ],
        "one run answers every item it admitted"
    );
    assert_eq!(parts.calls.load(Ordering::SeqCst), 1, "one model call");
    for id in &obligations {
        assert_eq!(
            ingress.state(id).await.expect("the settled state"),
            Some(lash_core::store::ObligationState::Delivered),
            "the run's admission of the row delivered its ingress obligation"
        );
    }
}

/// Crashes a run's execution after its model call and before its commit:
/// the run is admitted and sealed, and nothing it did is committed.
struct CrashBeforeCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PreparedTurn {
            panic!("injected crash after the seal and before the commit");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// L-S4: a redrive of a shift request replays its admissions and seals, so
/// it mints no ownership. The request's shift crashes after its seal and
/// before its commit, and its redrive replays the recorded admission and
/// seal: the epoch transitions exactly once, under the first admission, and
/// the run commits once.
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
    let parts = ShiftParts::new(prefix, "replay-ownership", &effect_host, &stores, 8).await;
    let input = parts
        .enqueue("ask once", Some("replay-ownership-run"))
        .await;
    let request = parts.request("replay-ownership-shift");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ShiftOutcome>();
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
                let outcome = lash_core::shift::work_session(&mut runtime, &scope, &request)
                    .await
                    .expect("the redriven shift runs");
                assert!(!crash, "the crash fires before the run commits");
                let _ = tx.send(outcome);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("shift-law-driver"),
            )),
            attempt(true),
            attempt(false),
        )
        .await;
    let outcome = rx.recv().await.expect("the redrive ran the shift");
    assert!(
        matches!(outcome.ran.as_slice(), [RunOutcome::Committed { run, .. }] if run.as_str() == "replay-ownership-run"),
        "the redrive executes the crashed run to its commit: {outcome:?}"
    );
    assert_eq!(outcome.stop, ShiftStop::Idle);
    let epoch = parts.epoch().await;
    assert_eq!(epoch.epoch, 1, "the redrive never raises the epoch again");
    assert_eq!(
        epoch.admission().map(|id| id.as_str()),
        Some("replay-ownership-shift#0"),
        "the epoch stays sealed by the first admission"
    );
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("replay-ownership-run"))],
        "the run commits once"
    );
}

/// L-S7: admission precedes the first effect: when the run's first model
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
    let mut parts = ShiftParts::new(prefix, "admission-first", &effect_host, &stores, 8).await;
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
                        .shift_epoch(&session_id)
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
    parts.host.providers.models = crate::testing::standard_test_llm_profiles(model.into_handle());
    parts.enqueue("ask", Some("admission-first-run")).await;
    let request = parts.request("admission-first-shift");
    let outcome: ShiftOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs")
        })
    })
    .await;
    assert_eq!(outcome.ran.len(), 1, "{outcome:?}");
    assert_eq!(
        *sealed_at_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![1],
        "the run's first model call ran under its sealed admission"
    );
}

/// L-S9: an admission whose seal never ran (a reset discarded it) holds
/// nothing: a fresh shift admits and seals anew, and the stale admission's
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
    let parts = ShiftParts::new(prefix, "reset-admission", &effect_host, &stores, 8).await;
    parts.enqueue("ask", Some("reset-admission-run")).await;
    let stale = parts.request("reset-admission-stale");
    let fresh = parts.request("reset-admission-fresh");
    let (outcome, late) = on_tier(&runner, &parts, move |mut runtime, scope| {
        let stale = stale.clone();
        let fresh = fresh.clone();
        Box::pin(async move {
            let stale = admitted(
                lash_core::shift::admit_shift(&mut runtime, &scope, &stale, 0, None)
                    .await
                    .expect("admit the stale shift"),
            );
            let outcome = lash_core::shift::work_session(&mut runtime, &scope, &fresh)
                .await
                .expect("the fresh shift runs");
            let late = lash_core::shift::execute_admitted_run(&mut runtime, &scope, stale)
                .await
                .expect("the stale run ends without an abort");
            (outcome, late)
        })
    })
    .await;
    assert_eq!(outcome.ran.len(), 1, "{outcome:?}");
    assert!(
        matches!(late, RunOutcome::Refused { .. }),
        "a stale admission's late seal is superseded: {late:?}"
    );
    let epoch = parts.epoch().await;
    assert_eq!(epoch.epoch, 1);
    assert_eq!(
        epoch.admission().map(|id| id.as_str()),
        Some("reset-admission-fresh#0")
    );
    assert_eq!(parts.calls.load(Ordering::SeqCst), 1);
}

/// L-S10: a parked run blocks admission: while the session's park stands, a
/// fresh shift admits nothing and runs nothing, and the pending work stays
/// accepted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn parked_run_blocks_admission(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "parked-run", &effect_host, &stores, 8).await;
    let input = parts
        .enqueue("waits behind the park", Some("after-the-park"))
        .await;
    parts
        .store
        .record_turn_park(&crate::store::TurnParkWrite {
            session_id: parts.session_id.clone(),
            turn_id: TurnId::from("parked-run"),
            reason: crate::store::ParkReason::ReplayDivergence {
                message: "the shift-admission law parks this run".to_string(),
            },
            at_ms: 1,
            origin: crate::store::TurnParkOrigin::Refusal,
            build_generation: None,
        })
        .await
        .map(lash_core::store::StoreTransition::into_record)
        .expect("record the park");
    let request = parts.request("parked-run-shift");
    let outcome: ShiftOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift stops at the park")
        })
    })
    .await;
    assert!(outcome.ran.is_empty(), "{outcome:?}");
    assert!(
        matches!(&outcome.stop, ShiftStop::Parked(park) if park.run.as_str() == "parked-run"),
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

/// The paths in `value` whose key names a shift epoch or fence.
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
/// shift builds from its admission — the admission itself, the seal and the
/// run's admission — carries no shift fence and no epoch, except the seal's
/// `observed_epoch`: the compare-and-set input it decodes from the recorded
/// admission verdict, which every replay reproduces. The fence the seal
/// yields rides its outcome only, and the run's turn effects are built
/// without the admission, so nothing a later shift of the same run issues
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
    let parts = ShiftParts::new(prefix, "fence-envelope", &effect_host, &stores, 8).await;
    let input = parts.enqueue("ask", Some("fence-envelope-run")).await;
    let request = parts.request("fence-envelope-shift");
    let admit_request = request.clone();
    let admission = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = admit_request.clone();
        Box::pin(async move {
            admitted(
                lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                    .await
                    .expect("admit the run"),
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
            crate::RuntimeEffectCommand::AdmitShift {
                request: Box::new(lash_core::engine::AdmitRequest {
                    session: request.session.clone(),
                    request: request.request.clone(),
                    build_generation: parts
                        .host
                        .backend()
                        .build_generation()
                        .expect("the engine generation is bound")
                        .clone(),
                }),
            },
        ),
        (
            "seal",
            crate::RuntimeEffectCommand::SealShiftAdmission {
                admitted: Box::new(admission),
            },
        ),
        (
            "admit_run",
            crate::RuntimeEffectCommand::AdmitRun {
                head: crate::store::AdmittedHead::Input(input),
            },
        ),
    ];
    for (name, command) in commands {
        let value = serde_json::to_value(&command).expect("serialize the shift command");
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

/// A session store whose worker dies once right after the run admission
/// committed, before the effect journal records the admission's outcome
/// (FIG-3840). It keeps every admission it returned, with the shift
/// epoch that asked for it.
struct CrashAfterAdmission {
    inner: Arc<dyn crate::RuntimeStore>,
    fired: AtomicUsize,
    results: std::sync::Mutex<Vec<(u64, crate::store::RunAdmission)>>,
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for CrashAfterAdmission {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_run(
        &self,
        request: &crate::store::AdmitRunRequest,
    ) -> Result<Option<crate::store::RunAdmission>, crate::StoreError> {
        let admission = self.inner.admit_run(request).await?;
        if let Some(shift) = &admission {
            self.results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((request.fence.epoch(), shift.clone()));
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

/// A run whose worker dies after the store committed its admission, but
/// before the journal recorded the admission's outcome, is redriven by a
/// fresh worker under the same recorded shift admission on exactly the
/// composition, base and executable generation the admission committed
/// (FIG-3840, FIG-3927 N9 (a)). An input that arrives in the window never
/// widens the recorded prefix.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_run_admission_survives_a_worker_crash_without_widening(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts =
        ShiftParts::new(prefix, "admission-commit-crash", &effect_host, &stores, 8).await;
    parts.compose_inputs();
    let crash = Arc::new(CrashAfterAdmission {
        inner: Arc::clone(&parts.store),
        fired: AtomicUsize::new(0),
        results: std::sync::Mutex::new(Vec::new()),
    });
    parts.store = Arc::clone(&crash) as Arc<dyn crate::RuntimeStore>;
    let first = parts.enqueue("first", Some("admission-commit-run")).await;
    let second = parts.enqueue("second", None).await;
    let request = parts.request("admission-commit-shift");
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let request = request.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let admitted = admitted(
                    lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                        .await
                        .expect("admit the run"),
                );
                let _ =
                    lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted).await;
                panic!("the admission crash must interrupt the run");
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
                    lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                        .await
                        .expect("readmit the run"),
                );
                lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
                    .await
                    .expect("redrive the run");
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
        "the retried admission reuses its sealed shift epoch"
    );
    assert_eq!(
        crashed.input_ids(),
        vec![first.clone(), second.clone()],
        "the crashed worker admitted the prefix queued before it"
    );
    assert_eq!(
        serde_json::to_value(successor).expect("encode the successor's admission"),
        serde_json::to_value(crashed).expect("encode the crashed admission"),
        "the successor executes the recorded composition, base and generation"
    );
    assert_eq!(
        parts.applications().await,
        vec![
            (first, TurnId::from("admission-commit-run")),
            (second, TurnId::from("admission-commit-run"))
        ],
        "the late input must not enter the crashed run's recorded admission"
    );
}

struct NoReplayRepairRead {
    inner: Arc<dyn crate::RuntimeStore>,
    after_first: AtomicUsize,
    replay_reads: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for NoReplayRepairRead {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        lease: &crate::store::ShiftFence,
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

/// A committed run redriven on its journal replays the admission that included
/// orphan repair. The repair's store read runs only on first execution;
/// a replay honours the recorded head verdict without a live head check.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_run_replays_its_recorded_repair(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts =
        ShiftParts::new(prefix, "recorded-head-inspection", &effect_host, &stores, 1).await;
    let read_guard = Arc::new(NoReplayRepairRead {
        inner: Arc::clone(&parts.store),
        after_first: AtomicUsize::new(0),
        replay_reads: AtomicUsize::new(0),
    });
    parts.store = Arc::clone(&read_guard) as Arc<dyn crate::RuntimeStore>;
    parts.enqueue("one answer", Some("recorded-head-run")).await;
    let request = parts.request("recorded-head-shift");
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
                    lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                        .await
                        .expect("admit the run"),
                );
                lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
                    .await
                    .expect("first run commits");
                read_guard.after_first.store(1, Ordering::SeqCst);
                panic!("worker died after committing the run");
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
                    lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                        .await
                        .expect("replay admission"),
                );
                lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
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

/// A store that does not answer at a run's admission is that attempt's
/// fault, never the admission's recorded outcome (FIG-3600 review HIGH-3).
/// The run is re-admitted first by every later shift, so a recorded fault
/// would replay under its admission key forever and wedge the session.
/// Instead the attempt aborts open, its retry reads the run's admission
/// back (rows the faulted attempt bound stay bound to the run), and the
/// run commits once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_store_fault_at_the_run_admission_is_retried_not_recorded(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for (after, law) in [
        (false, "admission-fault-at-admission"),
        (true, "admission-fault-after-admission"),
    ] {
        let mut parts = ShiftParts::new(prefix, law, &effect_host, &stores, 8).await;
        let script = lash_core::testing::Script::new();
        let op = lash_core::testing::StoreOp::admit_run;
        if after {
            script.on(op).after().lose_reply();
        } else {
            script.on(op).before().fail(|| crate::StoreError::Contended);
        }
        parts.store = script.wrap(law, Arc::clone(&parts.store));
        let input = parts.enqueue("ask once", Some("admission-fault-run")).await;
        let request = parts.request("admission-fault-shift");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<RunOutcome, String>>();
        let attempt: crate::ConformanceTurnAttempt = {
            let parts = parts.clone();
            Arc::new(move |scope| {
                let parts = parts.clone();
                let request = request.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let mut runtime = parts.runtime().await;
                    let verdict =
                        lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                            .await
                            .expect("admit the run");
                    let lash_core::engine::AdmitVerdict::Admit(admitted) = verdict else {
                        // The engine already retried the faulted execution to
                        // its commit: this run finds nothing to admit.
                        return crate::ConformanceTurnEnd::Settled;
                    };
                    match lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
                        .await
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
            TurnId::from("shift-law-driver"),
        ));
        // The faulted execution stays open; the next run of the same scope
        // is its retry, unless the tier's engine already retried it.
        runner.run_turn(scope.clone(), Arc::clone(&attempt)).await;
        runner.run_turn(scope, attempt).await;
        let mut outcomes = Vec::new();
        while let Ok(outcome) = rx.try_recv() {
            outcomes.push(outcome);
        }
        let committed: Vec<_> = outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().ok())
            .collect();
        assert!(
            matches!(committed.as_slice(), [RunOutcome::Committed { .. }]),
            "{law}: the retry commits the run exactly once: {outcomes:?}"
        );
        assert_eq!(
            parts.calls.load(Ordering::SeqCst),
            1,
            "{law}: one model call"
        );
        assert_eq!(
            parts.epoch().await.epoch,
            1,
            "{law}: one shift-epoch transition"
        );
        assert_eq!(
            parts.applications().await,
            vec![(input, TurnId::from("admission-fault-run"))],
            "{law}: the input is applied once"
        );
    }
}

/// ADR 0101 §4 (FIG-3892): the command lane drains first at every turn
/// boundary, and an input run's admission is that boundary. A session
/// command enqueued after the shift admitted the run, before the run's
/// own admission step, never holds the run's head back: the run takes the prefix ahead of the
/// command and commits, while an input enqueued after the command waits.
/// The next shift applies the command before that input runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_command_enqueued_after_an_input_runs_admission_waits_for_the_next_boundary(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "command-after-admission", &effect_host, &stores, 8).await;
    let head = parts
        .enqueue("ahead of the command", Some("command-after-admission-run"))
        .await;
    let request = parts.request("command-after-admission-shift");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<RunOutcome, String>>();
    let attempt: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let verdict =
                    lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                        .await
                        .expect("admit the input run");
                let admitted = admitted(verdict);
                // The command and a later input arrive between the run's
                // shift admission and its own. Both are keyed, so a tier that
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
                match lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted).await {
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
    // The run's first execution must commit: an admission the command held back
    // would fail it retryably, and the tier would retry it forever.
    let executing = tokio::spawn({
        let runner = Arc::clone(&runner);
        let scope = driver_scope(&parts);
        async move { runner.run_turn(scope, attempt).await }
    });
    // A wedged run never ends its first execution: bound the wait so the
    // wedge fails here rather than at the harness's test timeout.
    let first = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
        .await
        .expect("the run's first execution ends instead of retrying its admission forever")
        .expect("the run's first execution ended");
    assert!(
        matches!(
            &first,
            Ok(RunOutcome::Committed { run, .. })
                if run.as_str() == "command-after-admission-run"
        ),
        "the admitted run takes its head past the later command and commits: {first:?}"
    );
    executing.await.expect("the tier settles the shift");
    let run = TurnId::from("command-after-admission-run");
    assert_eq!(
        parts.applications().await,
        vec![(head.clone(), run.clone())],
        "the input behind the command waits for the next boundary"
    );
    let pending = parts
        .store
        .list_open_queued_work(&parts.session_id)
        .await
        .expect("read the command lane");
    assert_eq!(pending.len(), 1, "the command is still open: {pending:?}");

    let next = parts.request("command-after-admission-next");
    let outcome: ShiftOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let next = next.clone();
        Box::pin(async move {
            lash_core::shift::work_session(&mut runtime, &scope, &next)
                .await
                .expect("the next shift runs")
        })
    })
    .await;
    assert_eq!(outcome.stop, ShiftStop::Idle, "{outcome:?}");
    assert!(
        matches!(
            outcome.ran.first(),
            Some(RunOutcome::Applied { run })
                if run.as_str().starts_with("shift-commands:")
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

/// K8 (binding Q2, FIG-4888): a host's plugin task is a tool-bearing
/// operation, and it runs as its own logical Run. The lane's command run
/// applies the command ahead of the task and stops at the task; the next
/// admission names the task's operation run, named by the operation alone.
/// An admission under a later shift request (a redrive after a crash) names
/// the same operation and the same run, so the session's keyed turn service
/// reaches the same journal owner.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_task_is_admitted_as_its_own_operation_run(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "operation-run", &effect_host, &stores, 8).await;
    let task_scopes = Arc::new(std::sync::Mutex::new(Vec::new()));
    parts
        .plugins
        .push(operation_task_plugin(Arc::clone(&task_scopes)));
    let command = |command: crate::SessionCommand, key: &str| {
        crate::QueuedWorkBatchDraft::new(
            &parts.session_id,
            crate::DeliveryPolicy::EarliestSafeBoundary,
            command,
        )
        .with_source_key(key)
    };
    parts
        .store
        .enqueue_queued_work(command(
            crate::SessionCommand::RefreshToolCatalog {
                reason: "ahead of the task".to_string(),
            },
            "operation-run-refresh",
        ))
        .await
        .expect("enqueue the command ahead of the task");
    let task = parts
        .store
        .enqueue_queued_work(command(
            crate::SessionCommand::RunPluginTask {
                name: <OperationTask as crate::plugin::PluginOperation>::NAME.to_string(),
                args: serde_json::json!({}),
            },
            "operation-run-task",
        ))
        .await
        .expect("enqueue the task");

    let commands = parts.request("operation-run-commands");
    let (work, ran) = on_tier(&runner, &parts, move |mut runtime, scope| {
        let commands = commands.clone();
        Box::pin(async move {
            let admitted = admitted(
                lash_core::shift::admit_shift(&mut runtime, &scope, &commands, 0, None)
                    .await
                    .expect("admit the command lane"),
            );
            let work = admitted.work().clone();
            let ran = lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
                .await
                .map_err(|abort| abort.into_error().to_string());
            (work, ran)
        })
    })
    .await;
    assert!(
        matches!(work, lash_core::engine::AdmittedWork::Commands { .. }),
        "the command ahead of the task runs in the command run: {work:?}"
    );
    assert!(
        matches!(&ran, Ok(RunOutcome::Applied { run }) if run.as_str().starts_with("shift-commands:")),
        "{ran:?}"
    );
    let open: Vec<_> = parts
        .store
        .list_open_queued_work(&parts.session_id)
        .await
        .expect("read the command lane")
        .into_iter()
        .map(|batch| batch.batch_id)
        .collect();
    assert_eq!(
        open,
        vec![task.batch_id.clone()],
        "the command run applies the command ahead and stops at the task"
    );

    let operation = lash_core::tool_run::OperationRun {
        session_id: parts.session_id.clone(),
        operation_id: task.batch_id.to_string(),
    };
    for request in ["operation-run-first", "operation-run-redrive"] {
        let request = parts.request(request);
        let admitted = admitted(
            on_tier(&runner, &parts, move |mut runtime, scope| {
                let request = request.clone();
                Box::pin(async move {
                    lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                        .await
                        .expect("admit the task")
                })
            })
            .await,
        );
        assert_eq!(
            admitted.work(),
            &lash_core::engine::AdmittedWork::Operation {
                operation: task.batch_id.clone(),
            },
            "the task at the lane's head is an operation"
        );
        assert_eq!(
            admitted.run(),
            &operation.run_id(),
            "every admission of the operation names its one run"
        );
        assert_eq!(admitted.operation(), Some(operation.clone()));
    }

    // The operation run executes the task: the task's effect runs under the
    // operation's session-operation scope (its identity bytes unchanged),
    // the task settles, and the run ends applied.
    let request = parts.request("operation-run-execute");
    let ran = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            let admitted = admitted(
                lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                    .await
                    .expect("admit the task"),
            );
            lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
                .await
                .map_err(|abort| abort.into_error().to_string())
        })
    })
    .await;
    assert_eq!(
        ran,
        Ok(RunOutcome::Applied {
            run: operation.run_id()
        }),
        "the operation run applies its task"
    );
    let scopes = task_scopes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(!scopes.is_empty(), "the task ran");
    assert!(
        scopes
            .iter()
            .all(|scope| scope == operation.opener().admitted_scope().scope()),
        "the task's effects run under the operation's session-operation scope: {scopes:?}"
    );
    assert!(
        parts
            .store
            .queued_work_batch_completion(&parts.session_id, task.batch_id.as_str())
            .await
            .expect("read the task's settlement")
            .is_some(),
        "the task settled through its command's commit"
    );
}

/// The operation law's task: one durable sleep through the controller its
/// operation run lends it, recording the scope it ran under.
struct OperationTask;

impl crate::plugin::PluginOperation for OperationTask {
    const NAME: &'static str = "conformance_operation_task";
    const DESCRIPTION: &'static str = "Sleep once through the operation's controller.";
    const SESSION_PARAM: crate::plugin::SessionParam = crate::plugin::SessionParam::Required;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
    type Error = String;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(_: &Self::Error) -> lash_sansio::PluginFailureClass {
        lash_sansio::PluginFailureClass::Terminal
    }
}

impl crate::plugin::PluginTask for OperationTask {}

fn operation_task_plugin(
    scopes: Arc<std::sync::Mutex<Vec<crate::ExecutionScope>>>,
) -> Arc<dyn crate::plugin::PluginFactory> {
    Arc::new(crate::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("conformance-operation-task"),
        crate::facade_support::PluginSpec::new().with_plugin_task_value::<OperationTask, _, _>(
            move |ctx, _args| {
                let scopes = Arc::clone(&scopes);
                async move {
                    let controller = ctx.scoped_effect_controller;
                    let scope = controller.execution_scope().clone();
                    scopes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(scope.clone());
                    controller
                        .execute_effect(
                            crate::RuntimeEffectEnvelope::new(
                                crate::RuntimeEffectInvocation::new(
                                    crate::EffectAddress::new(scope, "operation-task-sleep")
                                        .map_err(|error| error.to_string())?,
                                    crate::RuntimeAttribution::none(),
                                    "operation-task-sleep",
                                ),
                                crate::RuntimeEffectCommand::Sleep {
                                    spec: crate::SleepSpec::For { duration_ms: 1 },
                                },
                            ),
                            crate::RuntimeEffectLocalExecutor::unavailable(),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(serde_json::json!({"slept": true}))
                }
            },
        ),
    ))
}

/// What an idle session's admission takes first: `queued` or `input:<id>`.
async fn idle_admission(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ShiftParts,
    request: &str,
) -> String {
    let request = parts.request(request);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            match lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None).await {
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
    let wake = |parts: &ShiftParts, sequence: u64| {
        crate::conformance::helpers::process_wake_work(
            &parts.session_id,
            "idle-order",
            sequence,
            "a wake",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        )
    };

    let wake_first =
        ShiftParts::new(prefix, "idle-order-wake-first", &effect_host, &stores, 8).await;
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
        ShiftParts::new(prefix, "idle-order-input-first", &effect_host, &stores, 8).await;
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
        ShiftParts::new(prefix, "idle-order-command-last", &effect_host, &stores, 8).await;
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

/// Accept `items` in order on a fresh law session, shift it to idle, and
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
    let mut parts = ShiftParts::new(prefix, law, effect_host, stores, admission_bound).await;
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
    parts.host.providers.models = crate::testing::standard_test_llm_profiles(model.into_handle());

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
    let request = parts.request(&format!("{law}-shift"));
    let outcome: ShiftOutcome = on_tier(runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs")
        })
    })
    .await;
    assert_eq!(outcome.stop, ShiftStop::Idle, "{law}: {outcome:?}");
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
/// run, an input run or a checkpoint admission did the taking.
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
        // An input heads the lane: its run takes the input, not the input
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
