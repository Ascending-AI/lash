//! Paused children of root-opened effect groups (FIG-4630), against the same
//! handlers on the double and a live server.
//!
//! A group child runs in an invocation of its own. When the engine stops
//! retrying it, what happens to it follows what its group still needs:
//!
//! - a child whose position is not seated parks the root its group runs for,
//!   and the park records the child's invocation, in every phase of the
//!   group: a `RunToCompletion` loser of a closed group keeps its scope live
//!   until it settles;
//! - a child whose position is seated, or whose group retired, is released;
//!   a close or retirement that cancels it resumes it to release itself, so
//!   the pass is what releases one no signal reached;
//! - a redrive resumes exactly the children its park recorded, and reads
//!   nothing else.
//!
//! Each law opens its groups at the index, as a root's controller does, with
//! a timer child no executor routes: its attempts fail until the engine
//! pauses it. Routing the timer is what an operator's fix is.
use super::effect_group_conformance::{
    HarnessServer, LiveConformanceHarness, await_group_wait, index_state, overwrite_index_state,
    replace_index_state, witness_dispatch_route, witness_key, witness_membership,
};
use super::root_control_witnesses::{attach_whole_drive, recorded_admission};
use crate::effect_group::{
    EffectGroupCloseRequest, EffectGroupCloseResponse, EffectGroupCommitChildRequest,
    EffectGroupCommitChildResponse, EffectGroupCommittedFinal, EffectGroupDispatchRequest,
    EffectGroupNotice, EffectGroupNotification, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupPhase, EffectGroupProbeResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupSettlementTerminal, EffectGroupShape,
};
use crate::ingress::RestateInvocationLifecycle;
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::{
    EffectAddress, ExecutionScope, GroupExecutors, GroupWakePolicy, LoserPolicy,
    RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, SessionDriver, SessionId, SessionWorkEngine,
    TurnId,
};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The deployment's resolver: an atomic child answers at once, and a timer
/// child is routed only once the law restores it.
#[derive(Default)]
struct LawExecutors {
    timers_routed: AtomicBool,
}

impl GroupExecutors for LawExecutors {
    /// The deployment runs timers: a worker that carries none right now
    /// fails the child's attempt, it does not refuse its group.
    fn routes(&self, _: &RuntimeEffectEnvelope) -> bool {
        true
    }

    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        match &envelope.command {
            RuntimeEffectCommand::Sleep { .. } => {
                self.timers_routed.load(Ordering::SeqCst).then(|| {
                    RuntimeEffectLocalExecutor::sleep(tokio_util::sync::CancellationToken::new())
                        .with_turn_cancel_observation(false)
                })
            }
            RuntimeEffectCommand::LanguageRuntimeValue { .. } => {
                Some(RuntimeEffectLocalExecutor::testing(|_| async {
                    Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                        value: serde_json::json!({ "winner": true }),
                    })
                }))
            }
            _ => None,
        }
    }
}

/// The deployment's session driver. A law is its roots' controller: it opens
/// their groups at the index itself, so the driver admits no root and a
/// drive of a law's session ends idle.
struct LawDriver;

#[async_trait::async_trait]
impl SessionDriver for LawDriver {
    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &DriveRequest,
        admitting_generation: &BuildGeneration,
        ordinal: u32,
        _draining: Option<&BuildGeneration>,
    ) -> Result<AdmitVerdict, DriveAbort> {
        recorded_admission(
            &controller,
            request,
            admitting_generation,
            ordinal,
            || async { Ok(AdmitVerdict::Idle) },
        )
        .await
    }

    async fn run_root(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _admitted: Admitted,
    ) -> RootRunEnd {
        unreachable!("the law's driver admits no root")
    }

    async fn close_root(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _session: &SessionId,
        _root: &TurnId,
    ) -> Result<(), DriveAbort> {
        unreachable!("the law's driver admits no root")
    }
}

/// One group a law's root opened, and its paused timer child.
struct Group {
    key: String,
    /// The paused timer child's invocation.
    paused: String,
}

struct Law {
    harness: LiveConformanceHarness,
    executors: Arc<LawExecutors>,
    work: crate::RestateSessionWork,
    /// The engine's installation of the [`LawDriver`], kept for the law's
    /// life.
    _driver: Arc<dyn SessionDriver>,
    factory: Arc<dyn lash_core::DeploymentStore>,
    ingress: crate::RestateIngressClient,
    admin: crate::RestateAdminClient,
}

impl Law {
    async fn start(server: HarnessServer) -> Self {
        let harness = LiveConformanceHarness::start_on(server).await;
        let executors = Arc::new(LawExecutors::default());
        harness.install_current_executors(Arc::clone(&executors) as Arc<dyn GroupExecutors>);
        let work = harness.session_work();
        Self {
            executors,
            _driver: work.install_session_driver(Arc::new(LawDriver)),
            work,
            factory: harness.law_stores().session_store_factory(),
            ingress: harness.ingress(),
            admin: harness.admin_client(),
            harness,
        }
    }

    /// Let the double's engine run: its retries and timers fire only when
    /// told. A live server runs on its own.
    async fn tick(&self) {
        if let Some(server) = self.harness.server_double() {
            server.settle().await;
            server.fire_next_timer();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    /// A live session holding one accepted input per root in `roots`.
    async fn session(&self, label: &str, roots: &[&str]) -> SessionId {
        let session = SessionId::fixture(format!(
            "paused-children-{label}-{}",
            self.harness.run_nonce()
        ));
        let store = lash_core::runtime::admit_session_view(
            &self.factory,
            &lash_core::SessionStoreCreateRequest {
                session_id: session.clone(),
                relation: lash_core::SessionRelation::Root,
                pending_observer_intents: vec![],
                config: lash_core::testing::mock_session_policy().into(),
                head: lash_core::SessionCreationHead::Config,
                owning_process_id: None,
            },
        )
        .await
        .expect("create the session");
        for root in roots {
            let input = store
                .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                    session.clone(),
                    lash_core::TurnInputIngress::next_turn(),
                    lash_core::TurnInput::text(*root),
                ))
                .await
                .expect("accept the root's input")
                .input_id;
            store
                .bind_root_inputs(&TurnId::fixture(*root), std::slice::from_ref(&input))
                .await
                .expect("bind the input to its root");
        }
        session
    }

    /// Open and dispatch a `RunToCompletion` group for root `root` of
    /// `session`: an atomic winner first when `winner`, then a timer child no
    /// executor routes. Returns once the engine paused the timer child.
    async fn group_with_paused_timer(
        &self,
        label: &str,
        session: &SessionId,
        root: &str,
        winner: bool,
    ) -> Group {
        let key = witness_key(label);
        let scope = ExecutionScope::turn(session.clone(), TurnId::fixture(root));
        let child = |position: usize, command| {
            RuntimeEffectEnvelope::new(
                RuntimeEffectInvocation::new(
                    EffectAddress::new(scope.clone(), format!("{key}:child:{position}"))
                        .expect("valid child address"),
                    RuntimeAttribution::none(),
                    "effect",
                ),
                command,
            )
        };
        let mut children = Vec::new();
        if winner {
            children.push(child(
                0,
                RuntimeEffectCommand::LanguageRuntimeValue {
                    operation: "winner".into(),
                },
            ));
        }
        children.push(child(
            children.len(),
            RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For { duration_ms: 1 },
            },
        ));
        let opened: EffectGroupOpenResponse = self
            .ingress
            .call_lash_object(
                "EffectGroupIndex",
                &key,
                "open",
                &EffectGroupOpenRequest {
                    shape: EffectGroupShape {
                        wake: GroupWakePolicy::First,
                        loser_disposition: LoserPolicy::RunToCompletion,
                        replay_keys: children
                            .iter()
                            .map(|child| child.invocation.effect_replay_key().to_owned())
                            .collect(),
                        opener: lash_core::AdmittedScope::turn(
                            session.clone(),
                            TurnId::fixture(root.to_string()),
                        ),
                    },
                    membership: witness_membership(&children),
                    dispatch_route: witness_dispatch_route(),
                    content_checked: false,
                },
            )
            .await
            .expect("the group opens");
        assert_eq!(
            opened,
            EffectGroupOpenResponse::OpenedFresh {
                dispatch_route: witness_dispatch_route()
            }
        );
        self.ingress
            .send_lash_workflow(
                &witness_dispatch_route(),
                &key,
                "run",
                &EffectGroupDispatchRequest {
                    group_key: key.clone(),
                },
            )
            .await
            .expect("the dispatcher is accepted");
        if winner {
            let rank = await_group_wait(&self.ingress, &key, EffectGroupNotice::Rank { rank: 1 });
            tokio::pin!(rank);
            let settled = loop {
                tokio::select! {
                    settled = &mut rank => break settled,
                    () = self.tick() => {}
                }
            };
            assert_eq!(settled, EffectGroupNotification::Rank, "the winner settles");
        }
        for _ in 0..3_000 {
            self.tick().await;
            if let Some(paused) = self
                .admin
                .paused_group_work(&crate::services::DEFAULT_NAMESPACE)
                .await
                .expect("list paused group work")
                .into_iter()
                .find(|invocation| {
                    invocation.target_service_key.as_deref() == Some(key.as_str())
                        && invocation.target_handler_name == "child"
                })
            {
                return Group {
                    key,
                    paused: paused.id,
                };
            }
        }
        panic!(
            "the engine did not pause the unrouted timer child of {key}: {:#?}",
            self.harness.server_double().map(|server| server
                .invocations()
                .into_iter()
                .filter(|view| view.target.contains(&key))
                .collect::<Vec<_>>())
        );
    }

    async fn close(&self, group: &Group, disposition: LoserPolicy) {
        let closed: EffectGroupCloseResponse = self
            .ingress
            .call_lash_object(
                "EffectGroupIndex",
                &group.key,
                "close",
                &EffectGroupCloseRequest { disposition },
            )
            .await
            .expect("the close answers");
        assert_eq!(closed, EffectGroupCloseResponse::Closed);
    }

    async fn unsettled(&self, group: &Group) -> usize {
        self.ingress
            .call_lash_object("EffectGroupIndex", &group.key, "unsettled_children", &())
            .await
            .expect("the index counts its unsettled children")
    }

    /// The invocation's lifecycle now; `None` once the engine holds it no
    /// more.
    async fn lifecycle(&self, invocation: &str) -> Option<RestateInvocationLifecycle> {
        self.admin
            .invocation_status(&crate::RestateInvocationId::new(invocation.to_owned()))
            .await
            .expect("read the invocation")
            .map(|status| status.status)
    }

    async fn is_paused(&self, invocation: &str) -> bool {
        self.lifecycle(invocation).await == Some(RestateInvocationLifecycle::Paused)
    }

    /// Wait until the resumed child `group` paused settled its position.
    async fn await_settled(&self, group: &Group) {
        for _ in 0..12_000 {
            self.tick().await;
            if self.unsettled(group).await == 0 {
                return;
            }
        }
        panic!("the resumed child of {} did not settle", group.key);
    }

    /// One whole park-reconcile pass: every page of the engine's listing.
    async fn reconcile(&self) -> ParkReconcileReport {
        let clock = lash_core::facade_support::SystemClock;
        let writer = lash_core::drive::StoreParkRecovery::new(self.factory.as_ref(), &clock);
        let mut whole = ParkReconcileReport::default();
        let mut after = None;
        loop {
            let page = self
                .work
                .control()
                .reconcile_parks(
                    &writer,
                    EnginePage {
                        budget: std::time::Duration::from_secs(20),
                        after,
                        limit: NonZeroUsize::new(64).expect("page"),
                    },
                )
                .await
                .expect("park pass");
            whole.parked.extend(page.parked);
            whole.released.extend(page.released);
            whole.released_work.extend(page.released_work);
            whole.failed.extend(page.failed);
            whole.attached += page.attached;
            whole.unchanged += page.unchanged;
            after = page.next;
            if after.is_none() {
                return whole;
            }
        }
    }

    /// Run passes until one releases `invocation`, within a bound: only a
    /// paused invocation is listed.
    async fn reconcile_until_released(&self, invocation: &str) -> ParkReconcileReport {
        let released = EnginePark::new(invocation);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let report = self.reconcile().await;
            if report.released_work.contains(&released) {
                return report;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no pass released the paused invocation {invocation}, now {:?}: {report:?}",
                self.lifecycle(invocation).await
            );
            self.tick().await;
        }
    }

    async fn park(&self, session: &SessionId) -> Option<TurnPark> {
        self.factory
            .load_turn_park(session)
            .await
            .expect("read the park")
    }

    /// Record `verb` on `park` and deliver its engine half.
    async fn verb(&self, park: &TurnPark, verb: RootVerb) -> ControlIntentState {
        self.deliver(park, verb).await.1
    }

    /// [`Self::verb`], also answering the drive its intent asks the session
    /// for.
    async fn deliver(
        &self,
        park: &TurnPark,
        verb: RootVerb,
    ) -> (DriveRequestId, ControlIntentState) {
        let intent = self
            .factory
            .open_root_intent(
                &RootIntentRequest {
                    session_id: park.session_id.clone(),
                    root: park.turn_id.clone(),
                    park: park.park_id,
                    verb,
                },
                5,
            )
            .await
            .expect("the store takes the verb");
        let scopes: Arc<dyn ScopeCloseSink> = Arc::new(NoScopeClose);
        let scope_close = Arc::new(lash_core::drive::ScopeCloseRelay::new(
            self.harness
                .law_stores()
                .obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&self.factory),
            Arc::clone(&scopes),
        ));
        let state = lash_core::drive::ControlIntentRelay::new(
            self.harness
                .law_stores()
                .obligation_ledger(ObligationKind::ControlIntent),
            Arc::clone(&self.factory),
            Arc::new(self.work.clone()) as Arc<dyn SessionWorkEngine>,
            scopes,
            scope_close,
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .deliver_intent(&intent)
        .await
        .expect("deliver the verb's engine half");
        (lash_core::drive::intent_drive_request(intent.id), state)
    }

    /// How `request`'s drive of `session` stopped.
    async fn drive_stop(&self, session: &SessionId, request: DriveRequestId) -> DriveStop {
        let attach = attach_whole_drive(&self.work, session, request);
        tokio::pin!(attach);
        for _ in 0..3_000 {
            tokio::select! {
                outcome = &mut attach => return outcome.expect("the drive answers").stop,
                () = self.tick() => {}
            }
        }
        panic!("the drive of {session} did not end");
    }

    fn handles(invocations: &[&str]) -> Vec<EnginePark> {
        invocations.iter().copied().map(EnginePark::new).collect()
    }
}

/// A `RunToCompletion` loser the engine paused after its group closed still
/// has to settle: until it does its group counts an unsettled child, which
/// keeps the opener's scope live. It parks the root its group runs for, the
/// park's redrive resumes it, and once it settled the group retires.
///
/// Red before FIG-4630: a closed group answered `Released` for every child,
/// so the pass left the loser paused and unparked for good.
async fn paused_loser(server: HarnessServer) {
    let law = Law::start(server).await;
    let session = law.session("loser", &["root"]).await;
    let group = law
        .group_with_paused_timer("closed-loser", &session, "root", true)
        .await;
    law.close(&group, LoserPolicy::RunToCompletion).await;
    assert_eq!(
        law.unsettled(&group).await,
        1,
        "the closed group still waits on its loser"
    );

    let report = law.reconcile().await;
    let target = ParkTarget::RootChild {
        session: session.clone(),
        root: TurnId::from("root"),
    };
    assert!(
        report.parked.contains(&target),
        "the paused loser parks the root that owns its drain: {report:?}"
    );
    let park = law.park(&session).await.expect("the root is parked");
    assert_eq!(park.turn_id, TurnId::from("root"));
    assert!(matches!(
        park.reason,
        ParkReason::EngineRetryExhausted { .. }
    ));
    assert_eq!(park.engine, None, "the root's own execution is not stopped");
    assert_eq!(park.children, Law::handles(&[&group.paused]));
    assert!(law.is_paused(&group.paused).await);
    let again = law.reconcile().await;
    assert!(
        !again.parked.contains(&target) && !again.released_work.contains(&park.children[0]),
        "a second pass over the same pause changes nothing: {again:?}"
    );
    assert_eq!(law.park(&session).await, Some(park.clone()));

    law.executors.timers_routed.store(true, Ordering::SeqCst);
    assert!(matches!(
        law.verb(&park, RootVerb::Redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    law.await_settled(&group).await;
    law.ingress
        .call_lash_workflow::<_, ()>(&witness_dispatch_route(), &group.key, "retire", &group.key)
        .await
        .expect("the settled group retires");
    let probe: EffectGroupProbeResponse = law
        .ingress
        .call_lash_object("EffectGroupIndex", &group.key, "probe", &())
        .await
        .expect("probe the retired group");
    assert!(matches!(
        probe,
        EffectGroupProbeResponse::Exists {
            phase: EffectGroupPhase::Retired,
            ..
        }
    ));
    law.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_loser_of_a_closed_group_parks_its_root_resumes_and_lets_the_group_retire() {
    paused_loser(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_paused_loser_of_a_closed_group_parks_its_root_resumes_and_lets_the_group_retire() {
    paused_loser(HarnessServer::Live).await;
}

/// A paused child whose position its group already seated owes nothing. A
/// close or a retirement cancels the children it seats, which resumes a
/// paused one, and a child that then misses its executor again releases
/// itself (FIG-4634). A seat no cancel announces leaves the child paused: here
/// the position is settled at the index while its invocation stays stopped.
/// The pass releases the invocation and parks nobody.
///
/// Red before FIG-4630: an open group answered that its opener waits on
/// every child, so the pass parked the root for a child whose seat was taken.
async fn seated_child(server: HarnessServer) {
    let law = Law::start(server).await;
    let session = law.session("seated", &["root"]).await;
    let group = law
        .group_with_paused_timer("seated", &session, "root", true)
        .await;
    let committed: EffectGroupCommitChildResponse = law
        .ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group.key,
            "commit_child",
            &EffectGroupCommitChildRequest {
                replay_key: format!("{}:child:1", group.key),
                committed: EffectGroupCommittedFinal::Held,
            },
        )
        .await
        .expect("the index commits the position");
    assert!(
        matches!(committed, EffectGroupCommitChildResponse::Committed { .. }),
        "{committed:?}"
    );
    let seated: EffectGroupRecordSettlementResponse = law
        .ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group.key,
            "record_settlement",
            &EffectGroupRecordSettlementRequest {
                position: 1,
                terminal: EffectGroupSettlementTerminal::Cancelled,
            },
        )
        .await
        .expect("the index seats the position");
    assert!(
        matches!(seated, EffectGroupRecordSettlementResponse::Recorded { .. }),
        "the paused child's position is seated: {seated:?}"
    );
    assert_eq!(law.unsettled(&group).await, 0);
    assert!(
        law.is_paused(&group.paused).await,
        "the seat reached no signal to the paused child"
    );

    let report = law.reconcile_until_released(&group.paused).await;
    assert!(
        !report.parked.iter().any(|target| matches!(
            target,
            ParkTarget::RootChild { session: parked, .. } if *parked == session
        )),
        "a seated child parks nobody: {report:?}"
    );
    assert!(
        !law.is_paused(&group.paused).await,
        "the released child is paused no more"
    );
    assert_eq!(
        law.park(&session).await,
        None,
        "a seated child parks nobody"
    );
    let again = law.reconcile().await;
    assert!(
        !again
            .released_work
            .contains(&EnginePark::new(group.paused.clone())),
        "nothing is left for a second pass: {again:?}"
    );
    law.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_child_of_a_seated_position_is_released() {
    seated_child(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_paused_child_of_a_seated_position_is_released() {
    seated_child(HarnessServer::Live).await;
}

/// A redrive resumes the children its park recorded and nothing else. A
/// child of the same root that paused after the pass is not the park's yet,
/// and an unrelated group whose index cannot be read is never asked: neither
/// is resumed, and neither fails the redrive. Once its index is repaired the
/// unrelated group is the next pass's: the pass parks its own root, and that
/// park's redrive settles it.
///
/// Red before FIG-4630: the redrive listed every paused child of the
/// deployment and asked each one's group for its opener, so the unreadable
/// group refused it.
async fn recorded_children(server: HarnessServer) {
    let law = Law::start(server).await;
    let session = law.session("recorded", &["root"]).await;
    let recorded = law
        .group_with_paused_timer("recorded", &session, "root", false)
        .await;
    let other_session = law.session("unrelated", &["root"]).await;
    let unrelated = law
        .group_with_paused_timer("unrelated", &other_session, "root", false)
        .await;
    let retained = index_state(law.harness.harness_admin(), &unrelated.key).await;
    overwrite_index_state(
        law.harness.harness_admin(),
        &unrelated.key,
        &serde_json::json!({ "not": "an effect-group index record" }),
    )
    .await;

    let report = law.reconcile().await;
    assert!(
        report
            .failed
            .iter()
            .any(|(cursor, _)| cursor.as_str() == unrelated.paused),
        "the unreadable group's child cannot be settled: {report:?}"
    );
    let park = law.park(&session).await.expect("the root is parked");
    assert_eq!(park.children, Law::handles(&[&recorded.paused]));
    assert_eq!(law.park(&other_session).await, None);
    // The same root opens another group whose child pauses after the pass.
    let later = law
        .group_with_paused_timer("later", &session, "root", false)
        .await;

    law.executors.timers_routed.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            law.verb(&park, RootVerb::Redrive).await,
            ControlIntentState::Acknowledged { .. }
        ),
        "an unreadable unrelated group never refuses the redrive"
    );
    law.await_settled(&recorded).await;
    for group in [&later, &unrelated] {
        assert!(
            law.is_paused(&group.paused).await,
            "the redrive resumes no child its park did not record: {}",
            group.key
        );
    }
    assert_eq!(law.unsettled(&later).await, 1);

    // The next pass finds the later child still stopped behind the settled
    // redrive, and re-parks the root with it.
    let next = law.reconcile().await;
    assert_eq!(next.attached, 1, "{next:?}");
    let reparked = law.park(&session).await.expect("the root is parked again");
    assert_eq!(reparked.park_id, park.park_id);
    assert_eq!(reparked.resume_intent, None);
    assert_eq!(
        reparked.children,
        Law::handles(&[&recorded.paused, &later.paused])
    );
    assert!(matches!(
        law.verb(&reparked, RootVerb::Redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    law.await_settled(&later).await;
    assert!(law.is_paused(&unrelated.paused).await);

    // The unrelated index is repaired. The next pass reads it and parks its
    // root with the child no redrive touched, and that root's redrive
    // settles the child.
    replace_index_state(law.harness.harness_admin(), &unrelated.key, retained).await;
    let repaired = law.reconcile().await;
    assert!(
        repaired.parked.contains(&ParkTarget::RootChild {
            session: other_session.clone(),
            root: TurnId::from("root"),
        }),
        "the repaired group's paused child parks its own root: {repaired:?}"
    );
    let unrelated_park = law
        .park(&other_session)
        .await
        .expect("the unrelated root is parked");
    assert_eq!(unrelated_park.children, Law::handles(&[&unrelated.paused]));
    assert!(matches!(
        law.verb(&unrelated_park, RootVerb::Redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    law.await_settled(&unrelated).await;
    law.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redrive_resumes_only_its_parks_recorded_children() {
    recorded_children(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_redrive_resumes_only_its_parks_recorded_children() {
    recorded_children(HarnessServer::Live).await;
}

/// A release ends a root and leaves its paused child to the next pass. The
/// drive it asks the session for ends with nothing held behind it. No
/// redrive in between resumes the child: not the redrive of the session's next
/// root, whose park records its own child, and not a resume of the released
/// root itself, which is handed nothing.
async fn released_root(server: HarnessServer) {
    let law = Law::start(server).await;
    let session = law.session("released", &["first", "second"]).await;
    let first = law
        .group_with_paused_timer("released-first", &session, "first", false)
        .await;
    law.reconcile().await;
    let park = law.park(&session).await.expect("the first root is parked");
    assert_eq!(
        (park.turn_id.clone(), park.children.clone()),
        (TurnId::from("first"), Law::handles(&[&first.paused]))
    );
    let (drive, released) = law.deliver(&park, RootVerb::Cancel).await;
    assert!(matches!(released, ControlIntentState::Acknowledged { .. }));
    assert_eq!(
        law.drive_stop(&session, drive).await,
        DriveStop::Idle,
        "the drive the release asked for ends"
    );
    assert!(
        law.factory
            .root_terminal(&session, &TurnId::from("first"))
            .await
            .expect("read the terminal")
            .is_some()
    );
    assert!(
        law.is_paused(&first.paused).await,
        "the release leaves the root's paused child to the next pass"
    );

    // The session's next root waits on a child of its own. Its park is
    // written as a pass would write it, without a pass that would reach the
    // released root's child first.
    let second = law
        .group_with_paused_timer("released-second", &session, "second", false)
        .await;
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::drive::StoreParkRecovery::new(law.factory.as_ref(), &clock);
    struct Stopped;
    #[async_trait::async_trait]
    impl StalledExecution for Stopped {
        async fn still_stopped(&self) -> Result<bool, EngineRefusal> {
            Ok(true)
        }
    }
    assert!(matches!(
        writer
            .record_engine_park(
                &ParkTarget::RootChild {
                    session: session.clone(),
                    root: TurnId::from("second"),
                },
                ParkReason::engine_retry_exhausted(8, None, "the child stopped".into()),
                EnginePark::new(second.paused.clone()),
                &Stopped,
            )
            .await
            .expect("park the second root"),
        EngineParkRecorded::Parked(_)
    ));
    let park = law.park(&session).await.expect("the second root is parked");
    assert_eq!(
        (park.turn_id.clone(), park.children.clone()),
        (TurnId::from("second"), Law::handles(&[&second.paused]))
    );

    law.executors.timers_routed.store(true, Ordering::SeqCst);
    assert!(matches!(
        law.verb(&park, RootVerb::Redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    law.await_settled(&second).await;
    assert!(
        law.is_paused(&first.paused).await,
        "a later redrive of the session never resumes the released root's child"
    );
    // A resume that names the released root is handed no child, and looks
    // for none: the engine holds nothing for it.
    assert_eq!(
        law.work
            .control()
            .resume_root(
                &RootRef {
                    session: session.clone(),
                    root: TurnId::from("first"),
                },
                None,
                &[],
            )
            .await
            .expect("the engine answers"),
        EngineAck::NothingHeld
    );
    assert!(law.is_paused(&first.paused).await);
    assert_eq!(law.unsettled(&first).await, 1);

    let report = law.reconcile().await;
    assert!(
        report.released.contains(&RootRef {
            session: session.clone(),
            root: TurnId::from("first"),
        }),
        "the next pass releases the ended root's child: {report:?}"
    );
    assert!(!law.is_paused(&first.paused).await);
    law.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_roots_paused_child_is_never_resumed_by_a_later_redrive() {
    released_root(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_released_roots_paused_child_is_never_resumed_by_a_later_redrive() {
    released_root(HarnessServer::Live).await;
}
