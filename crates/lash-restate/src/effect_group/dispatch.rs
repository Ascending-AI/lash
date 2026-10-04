use super::*;
use crate::controller::RestateRuntimeEffectController;
use lash_core::RuntimeEffectCommand;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupDispatchRequest {
    /// The object and workflow key. Everything else the dispatcher needs —
    /// the shape and the children it runs — is the index object's recorded
    /// state, never anything this request carries: a reopen's caller may
    /// offer different children, and the retained membership wins (ADR 0099
    /// §3).
    pub group_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupChildRequest {
    pub group_key: String,
    pub shape: EffectGroupShape,
    pub position: usize,
    pub envelope: RuntimeEffectEnvelope,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EffectGroupChildRunOutcome {
    Completed {
        outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    },
    Cancelled,
}

/// Whether the deployment that first took an admitted child could ever
/// execute it: recorded once, so every replay of the child takes the branch
/// its first attempt took (FIG-4550).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EffectGroupChildRoute {
    /// Nothing ruled the child out. An attempt that then finds no executor is
    /// on a deployment that does not carry the child now, and retries.
    Routable,
    /// The deployment that first took the child can never execute it.
    Unroutable {
        missing: lash_core::GroupChildCapability,
    },
}

#[derive(Clone)]
pub(crate) struct EffectGroupDispatchImpl {
    pub(super) executors: Arc<dyn GroupExecutors>,
    pub(super) ingress: RestateIngressClient,
    pub(super) admin: crate::RestateAdminClient,
    pub(super) authority_id: crate::ingress::RestateAuthorityId,
    pub(super) infinite_retry_policy: RunRetryPolicy,
    /// The catalog a session-scope child reads its owning session's state
    /// generation from at invocation entry (FIG-3619).
    pub(super) sessions: Arc<dyn lash_core::DeploymentStore>,
    /// The lane this dispatcher is bound under (FIG-3795): the route its
    /// self-calls — the child sends — address. An opener records the lane
    /// on the group's index record at `open`, so a group's dispatch and its
    /// children all run under the route the opener chose.
    pub(super) route: crate::services::ServiceRoute,
    /// The drain generation of the build this dispatcher runs: its journals
    /// lead with it (the generation sentinel), and a group a child opens
    /// dispatches on this build's lane.
    pub(super) build_generation: lash_core::engine::BuildGeneration,
}

impl EffectGroupDispatchImpl {
    pub(crate) fn new(
        host: &crate::RestateEffectHost,
        ingress: RestateIngressClient,
        admin: crate::RestateAdminClient,
        infinite_retry_policy: RunRetryPolicy,
        sessions: Arc<dyn lash_core::DeploymentStore>,
        route: crate::services::ServiceRoute,
        build_generation: lash_core::engine::BuildGeneration,
    ) -> Self {
        Self {
            executors: host.group_executors(),
            ingress,
            admin,
            authority_id: host.authority_id().clone(),
            infinite_retry_policy,
            sessions,
            route,
            build_generation,
        }
    }
}

impl std::fmt::Debug for EffectGroupDispatchImpl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EffectGroupDispatchImpl")
            .field("route", &self.route)
            .field("infinite_retry_policy", &self.infinite_retry_policy)
            .finish_non_exhaustive()
    }
}

impl EffectGroupDispatchImpl {}

/// The effect-group dispatcher: sends a group's children and runs each one.
///
/// A pinned service (FIG-3795): each build binds only its generation's lane,
/// and a group's opener records the lane
/// its dispatch runs under on the group's index record. Every child call
/// addresses the dispatcher's own lane, so a group's children run on the
/// build that opened it, however many newer builds are registered.
#[restate_sdk::workflow]
pub trait EffectGroupDispatch {
    async fn run(call: Call<EffectGroupDispatchRequest>) -> HandlerResult<Reply<()>>;

    #[shared]
    async fn preflight(
        call: Call<Vec<RuntimeEffectEnvelope>>,
    ) -> HandlerResult<Reply<Option<usize>>>;

    #[shared]
    async fn child(call: Call<EffectGroupChildRequest>) -> HandlerResult<Reply<()>>;

    #[shared]
    async fn retire(call: Call<String>) -> HandlerResult<Reply<()>>;
}

impl EffectGroupDispatchImpl {
    async fn run_child(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: &EffectGroupChildRequest,
    ) -> HandlerResult<()> {
        // The generation sentinel leads the journal (FIG-3795 §4.4): a child
        // runs on its dispatcher's lane, the build that opened its group.
        let recorded = crate::sentinel::record_generation!(&ctx, &self.build_generation)?;
        crate::sentinel::check_generation(&self.route.name(), &recorded, &self.build_generation)?;
        // FIG-3619: the owning session's generation is checked before
        // anything else this invocation does — before admission, before its
        // membership record, before an effect key is derived, and so before
        // its effect can be dispatched. The lane's builds all admit the
        // sessions its opener's build admitted: the session admission is an
        // input to the lane's generation (FIG-4454), and the marker moves
        // under no production writer. The gate is defence in depth: it
        // refuses only forged or operator-forced state. A refused child
        // settles with the typed refusal, which resolves the opener's rank
        // wait instead of stranding it; its effect never runs.
        if let Some(refusal) = session_generation_refusal(self.sessions.as_ref(), request).await? {
            return self
                .settle_unrun_child(ctx, request, UnrunChild::GenerationRefused(refusal))
                .await;
        }
        let own_id = ctx.invocation_id().to_string();
        let admission_request = EffectGroupAdmissionRequest {
            position: request.position,
            invocation_id: own_id,
        };
        let first = self
            .route
            .namespace()
            .effect_group_state(&ctx, request.group_key.clone())
            .admit_child(admission_request.clone())
            .call()
            .await?
            .into_body();
        let admission = match first {
            EffectGroupAdmissionResponse::Admitted => EffectGroupAdmissionResponse::Admitted,
            EffectGroupAdmissionResponse::AttachExpired => {
                // §8: this invocation is not the one that holds the position —
                // the index retains a different invocation id, or the
                // position's final is committed and the invocation that
                // committed it is gone (FIG-4454). The idempotency-keyed
                // dispatch minted this successor. The child is never executed
                // again: it seats a final the original committed, drained by
                // this child's own driver, or its typed failure where none is
                // committed.
                return self
                    .settle_unrun_child(ctx, request, UnrunChild::AttachExpired)
                    .await;
            }
            EffectGroupAdmissionResponse::CancelDecided => {
                return Ok(());
            }
            EffectGroupAdmissionResponse::Refused => {
                release_unadmitted_wait(
                    ctx,
                    self.authority_id.clone(),
                    self.build_generation.clone(),
                    self.route.namespace().clone(),
                    request,
                )
                .await?;
                return Ok(());
            }
            EffectGroupAdmissionResponse::Retired => {
                return Ok(());
            }
            EffectGroupAdmissionResponse::NotYetRecorded => {
                // The group's readiness is the child's admission notice
                // (FIG-4344), answered by the index that records it.
                await_group_notice!(
                    &ctx,
                    self.route.namespace(),
                    &request.group_key,
                    EffectGroupNotice::Ready
                )?;
                // The notice is notification only. Authorization always comes
                // from this one fresh, mapping-exact call after the wake.
                self.route
                    .namespace()
                    .effect_group_state(&ctx, request.group_key.clone())
                    .admit_child(admission_request)
                    .call()
                    .await?
                    .into_body()
            }
        };
        match admission {
            EffectGroupAdmissionResponse::Admitted => {}
            EffectGroupAdmissionResponse::AttachExpired => {
                return self
                    .settle_unrun_child(ctx, request, UnrunChild::AttachExpired)
                    .await;
            }
            EffectGroupAdmissionResponse::CancelDecided => {
                return Ok(());
            }
            EffectGroupAdmissionResponse::Refused => {
                release_unadmitted_wait(
                    ctx,
                    self.authority_id.clone(),
                    self.build_generation.clone(),
                    self.route.namespace().clone(),
                    request,
                )
                .await?;
                return Ok(());
            }
            EffectGroupAdmissionResponse::Retired => {
                return Ok(());
            }
            EffectGroupAdmissionResponse::NotYetRecorded => {
                return Err(TerminalError::new(format!(
                    "ADMIT notification for {} child {} did not produce a decisive fresh admission",
                    request.group_key, request.position
                ))
                .into());
            }
        }

        // The child's durable membership, written before anything of it can
        // run: the §4 boundary inside the child's own settle resolves its
        // group from this record — the Restate twin of the SQL tiers'
        // `group_key` column — never from a caller's assertion. A revoked
        // scope index refuses the record, and a child whose scope is gone
        // settles nowhere.
        let child_replay_key = request.envelope.invocation.effect_replay_key().to_string();
        let membership_admitted = self
            .route
            .namespace()
            .durable_wait_registry(
                &ctx,
                durable_wait_index_key_for_scope(request.envelope.invocation.execution_scope()),
            )
            .record_group_child(RestateDurableWaitGroupChildRequest {
                replay_key: child_replay_key.clone(),
                group_key: request.group_key.clone(),
            })
            .header(LASH_REPLAY_KEY_HEADER.to_string(), child_replay_key)
            .call()
            .await?
            .into_body();
        if !membership_admitted {
            return Ok(());
        }

        // A deployment that can never execute the child settles it with the
        // typed refusal naming what it lacks, so the opener's rank wait
        // resolves; a worker that only does not carry it now fails its attempt
        // below, which retries on a carrying one (FIG-4550, FIG-4590).
        if let EffectGroupChildRoute::Unroutable { missing } =
            self.record_child_route(&ctx, request).await?
        {
            return self
                .settle_unrun_child(ctx, request, UnrunChild::Unroutable(missing))
                .await;
        }

        // The child's durable cancel fact (ADR 0105 §4, FIG-3904), which the
        // group index holds (FIG-4344). No child races it at handler level: a
        // wait child races it as a journaled arm, and an atomic child watches
        // it inside its recorded body.
        let child_cancel = GroupChildCancel::new(
            self.ingress.clone(),
            self.route.namespace().clone(),
            request.group_key.clone(),
            request.position,
        );

        if matches!(
            request.envelope.command,
            RuntimeEffectCommand::Sleep { .. } | RuntimeEffectCommand::AwaitEvent { .. }
        ) {
            // A timer or durable-wait child is this invocation's own durable
            // wait (FIG-3397): a `ctx` timer or the Restate durable-wait
            // promise, journaled on the child's invocation — never a
            // wall-clock wait inside a recorded `ctx.run` body (ADR 0042).
            // The resolver answers wait *options*; the ctx-bound controller
            // is what reads them. The group index fenced the retained member
            // at open and this invocation is that member, so the wait runs as
            // a plain effect on the child's own journal rather than
            // re-carrying the membership the controller's command arms refuse.
            // It runs under the group's recorded opener, routed through the
            // host's stack like every other child kind, so a layer over that
            // host sees the wait (FIG-3780).
            let Some(executor) = self.executors.executor_for(&request.envelope) else {
                return self
                    .end_unrouted_child(&request.group_key, request.position, ctx.invocation_id())
                    .await;
            };
            let controller = RestateRuntimeEffectController::new(
                ctx,
                self.authority_id.clone(),
                self.build_generation.clone(),
            )
            .in_namespace(self.route.namespace().clone())
            .with_group_child_cancel(child_cancel);
            let envelope = RuntimeEffectEnvelope {
                group: None,
                ..request.envelope.clone()
            };
            let routed = lash_core::ScopedEffectController::borrowed(
                &controller,
                request.shape.opener.clone(),
            )
            .and_then(|scoped| self.executors.route_handler_child_controller(scoped));
            // The wait races the child's cancel fact as a journaled arm, so
            // a replay takes the arm its live run took.
            let outcome = child_run_outcome(match routed {
                Ok(scoped) => scoped.execute_effect(envelope, executor).await,
                Err(error) => Err(lash_core::RuntimeEffectControllerError::from(error)),
            });
            refuse_unrecorded_abort(request, &outcome)?;
            // A cancelled wait child does not release its own promise: the
            // index handler that decided the cancel, the close or the
            // retirement, released it before it resolved this cancel wait
            // (ADR 0099 §12, FIG-3630).
            return record_child_settlement(
                controller.context(),
                self.route.namespace(),
                request,
                outcome,
                None,
            )
            .await;
        }

        let cancellation = tokio_util::sync::CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let envelope = request.envelope.clone();
        let executors = Arc::clone(&self.executors);
        let dispatch = self.clone();
        let invocation_id = ctx.invocation_id().to_string();
        let group_key = request.group_key.clone();
        let position = request.position;
        let mut run = Box::pin(
            ctx.run(move || async move {
                let Some(executor) = executors.executor_for(&envelope) else {
                    dispatch
                        .end_unrouted_child(&group_key, position, &invocation_id)
                        .await?;
                    return Ok(Json(None));
                };
                let outcome = tokio::select! {
                    biased;
                    _ = run_cancellation.cancelled() => EffectGroupChildRunOutcome::Cancelled,
                    outcome = executor.execute(envelope) => {
                        EffectGroupChildRunOutcome::Completed { outcome }
                    }
                };
                if let EffectGroupChildRunOutcome::Completed {
                    outcome: Err(error),
                } = &outcome
                    && is_engine_retried_fault(error)
                {
                    // Failing the run step is what keeps the fault out of
                    // its journal: the step's retry policy runs it again.
                    return Err(std::io::Error::other(format!(
                        "effect group {group_key} child {position} aborted with a live fault, \
                         which is never its recorded outcome; the step retries: {error}"
                    ))
                    .into());
                }
                Ok(Json(Some(outcome)))
            })
            .name(format!(
                "lash:effect-group:{}:{}",
                request.group_key, request.position
            ))
            .retry_policy(self.infinite_retry_policy.clone()),
        );
        // The body's cancel is watched live on the shared ladder: a transient
        // fault of the watch retries, and a watch that gives up leaves the
        // body to run to its own end, so no fault of the watch ever drops it.
        // Whatever the body returns is its recorded outcome.
        let watch = child_cancel.watch();
        let watched =
            lash_core::retry_cancel_watch("an effect-group child's cancel", || watch.cancelled());
        tokio::pin!(watched);
        let Json(outcome) = tokio::select! {
            biased;
            outcome = &mut run => outcome?,
            watched = &mut watched => {
                match watched {
                    Ok(()) => cancellation.cancel(),
                    Err(lost) => tracing::warn!(
                        error = %lost,
                        group_key = %request.group_key,
                        position = request.position,
                        "an atomic effect-group child lost its cancel watch; its body runs to its own end"
                    ),
                }
                run.await?
            }
        };

        let Some(outcome) = outcome else {
            return Ok(());
        };
        record_child_settlement(&ctx, self.route.namespace(), request, outcome, None).await
    }

    /// Reads the index outside the child's journal on a routing miss. Admission
    /// and routing may replay accepted answers from before the seat became
    /// unnecessary. The engine's existing release path ends this invocation
    /// without replaying an executor's journal tail or seating again. A
    /// committed drain whose seat is still owed keeps retrying.
    async fn end_unrouted_child(
        &self,
        group_key: &str,
        position: usize,
        invocation_id: &str,
    ) -> HandlerResult<()> {
        let notice = self
            .ingress
            .call_lash_object::<_, Option<EffectGroupNotification>>(
                &self
                    .route
                    .namespace()
                    .stable(crate::LashService::EffectGroupState)
                    .name(),
                group_key,
                "child_cancel",
                &EffectGroupChildCancelRequest { position },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/child_cancel", error))?;
        if notice.is_some_and(|notice| notice.child_seat_is_no_longer_needed()) {
            self.admin
                .kill_invocation(&crate::RestateInvocationId::new(invocation_id))
                .await
                .map_err(|error| {
                    ingress_group_error("release an ended effect-group child", error)
                })?;
            return Ok(());
        }
        Err(std::io::Error::other(format!(
            "no executor currently routes effect group {group_key} child {position}; retry on a carrying deployment"
        ))
        .into())
    }

    /// Decides, once, whether the child is this lane's to run at all, and
    /// journals the answer.
    ///
    /// The resolver answers from what the child recorded and what its
    /// deployment wired, never from what the worker asked holds now, so the
    /// lane's workers agree (FIG-4590). The recorded route keeps every replay
    /// on the branch the first attempt took all the same. The step resolves
    /// no executor, so a resolver that hands each one out once still has it
    /// for the child's run.
    async fn record_child_route(
        &self,
        ctx: &SharedWorkflowContext<'_>,
        request: &EffectGroupChildRequest,
    ) -> HandlerResult<EffectGroupChildRoute> {
        let route = match self.executors.missing_capability(&request.envelope) {
            Some(missing) => EffectGroupChildRoute::Unroutable { missing },
            None => EffectGroupChildRoute::Routable,
        };
        let Json(route) = ctx
            .run(move || async move { Ok::<_, restate_sdk::errors::HandlerError>(Json(route)) })
            .name(format!(
                "lash:effect-group:route:{}:{}",
                request.group_key, request.position
            ))
            .await?;
        Ok(route)
    }

    /// Settles a child this invocation cannot run: its session's state
    /// generation is refused here, its attach expired (§8), or the deployment
    /// that first took it can never execute it (FIG-4550). The committed
    /// final wins (ADR 0099 §5): the typed refusal is offered to the §4 point
    /// and seats only where no final is committed. Where an earlier
    /// invocation committed one and ended before its seat, this invocation
    /// seats that final. A final it cannot realize is reported lost by name,
    /// never replaced by the refusal.
    async fn settle_unrun_child(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: &EffectGroupChildRequest,
        unrun: UnrunChild,
    ) -> HandlerResult<()> {
        let namespace = self.route.namespace();
        let refusal = unrun.refusal(request);
        let offered = EffectGroupCommittedFinal::Refusal {
            error: refusal.clone(),
        };
        match commit_child_final(&ctx, namespace, request, offered).await? {
            PointAnswer::Won => {
                seat_child_outcome(
                    &ctx,
                    namespace,
                    request,
                    EffectGroupChildRunOutcome::Completed {
                        outcome: Err(refusal),
                    },
                )
                .await
            }
            PointAnswer::Retired => Ok(()),
            PointAnswer::Taken { committed, .. } => {
                seat_committed_final(&ctx, namespace, request, committed, &unrun.why_unrealized())
                    .await
            }
        }
    }
}

impl EffectGroupDispatch for EffectGroupDispatchImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        call: Call<EffectGroupDispatchRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        // The generation sentinel leads the journal (FIG-3795 §4.4): a
        // journal another generation recorded parks, retryably, for a build
        // of that generation.
        let recorded = crate::sentinel::record_generation!(&ctx, &self.build_generation)?;
        crate::sentinel::check_generation(&self.route.name(), &recorded, &self.build_generation)?;
        let own_id = ctx.invocation_id().to_string();
        let adopted = self
            .route
            .namespace()
            .effect_group_state(&ctx, request.group_key.clone())
            .probe_and_adopt(EffectGroupAdoptRequest {
                invocation_id: own_id,
            })
            .call()
            .await?
            .into_body();
        let (shape, membership) = match adopted {
            EffectGroupProbeAdoptResponse::Adopted { shape, membership }
            | EffectGroupProbeAdoptResponse::AlreadyAdopted { shape, membership } => {
                (shape, membership)
            }
            EffectGroupProbeAdoptResponse::Ready
            | EffectGroupProbeAdoptResponse::Closed
            | EffectGroupProbeAdoptResponse::Retired => return Ok(Reply::at(wire, ())),
            EffectGroupProbeAdoptResponse::DifferentDispatcher
            | EffectGroupProbeAdoptResponse::UnknownGroup => {
                return Err(TerminalError::new(format!(
                    "effect-group dispatcher protocol defect for {}: {adopted:?}",
                    request.group_key
                ))
                .into());
            }
        };
        // The recorded membership is the child set, on first dispatch and on
        // every reopen alike. Decoding is pure — a membership entry that does
        // not decode is corruption the journal itself produced, a protocol
        // defect rather than a retryable error.
        shape.validate_membership(&membership)?;
        let children = (0..membership.0.len())
            .map(|position| membership.envelope(&request.group_key, position))
            .collect::<Result<Vec<_>, _>>()?;

        let executors = Arc::clone(&self.executors);
        let preflight_children = children.clone();
        let missing = ctx
            .run(move || async move {
                let mut missing = None;
                for (position, child) in preflight_children.iter().enumerate() {
                    if !executors.routes(child) && missing.is_none() {
                        missing = Some(position);
                    }
                }
                Ok(Reply::at(wire, missing))
            })
            .name("lash:effect-group:dispatch-preflight")
            .retry_policy(self.infinite_retry_policy.clone())
            .await?
            .into_body();
        if let Some(position) = missing {
            let outcome = self
                .route
                .namespace()
                .effect_group_state(&ctx, request.group_key.clone())
                .register_refusal(EffectGroupRefusalRequest {
                    reason: EffectGroupRefusal::NoExecutor { position },
                })
                .call()
                .await?
                .into_body();
            return match outcome {
                EffectGroupRegisterRefusalResponse::Refused
                | EffectGroupRegisterRefusalResponse::AlreadyRegistered
                | EffectGroupRegisterRefusalResponse::AlreadyClosed
                | EffectGroupRegisterRefusalResponse::Retired => Ok(Reply::at(wire, ())),
                EffectGroupRegisterRefusalResponse::UnknownGroup => {
                    Err(TerminalError::new(format!(
                        "dispatcher refused unknown effect group {}",
                        request.group_key
                    ))
                    .into())
                }
            };
        }

        // Children are `call` children of this dispatcher, issued eagerly and
        // awaited only after every one is recorded (ADR 0099 §2). Two
        // properties follow, and neither is available from `.send()`:
        //
        // * the pinned VM's implicit cancellation tracks `call` children and
        //   deliberately exempts one-way sends, so before this change it
        //   covered *zero* group children;
        // * the child's replay key is the idempotency key, so a redrive that
        //   re-issues this dispatch attaches to the invocation the first
        //   dispatch created rather than starting a fresh, unrelated one.
        //
        // Eager is load-bearing. `register_dispatch` below is what resolves
        // READY and releases the opener, so awaiting any child before that
        // point would deadlock the open. Issue all, record all and register in
        // one step, then hold.
        //
        // Every call is issued before any invocation id is awaited, and the
        // ids are recorded, and the group made ready, in one
        // `register_dispatch` (FIG-4088, FIG-4308): the children's own
        // admissions queue on the same exclusive index, behind one handler
        // rather than two. The engine
        // mints a call's id when it appends the call, so by the time the first
        // await suspends every id is journaled: the dispatch costs a fixed
        // number of suspensions whatever the width. Awaiting each id and
        // recording each dispatch in turn cost two per child, and on a
        // resumption that replays the journal, child i started only after
        // about 2i replays of a journal that grows with the width.
        let mut calls = Vec::with_capacity(children.len());
        for (position, envelope) in children.into_iter().enumerate() {
            let replay_key = shape.member_replay_key(position)?.to_string();
            // A child call goes to this dispatcher's own lane (FIG-3795):
            // the route the index recorded for the group's dispatch, which
            // this dispatch runs under, never a name recomputed from the
            // running build. The child's replay key is its idempotency key,
            // and Restate scopes that key by service name, so a retry can
            // only ever attach to the one child this lane started.
            let call = crate::services::routed_workflow::<_, _, ()>(
                &ctx,
                &self.route,
                request.group_key.clone(),
                "child",
                EffectGroupChildRequest {
                    group_key: request.group_key.clone(),
                    shape: shape.clone(),
                    position,
                    envelope,
                },
            )
            .idempotency_key(replay_key.clone())
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
            .call();
            calls.push((position, call));
        }
        let mut addresses = BTreeMap::new();
        for (position, call) in &calls {
            let invocation_id = call.invocation_handle().await?.invocation_id().to_owned();
            addresses.insert(*position, invocation_id);
        }
        let registered = self
            .route
            .namespace()
            .effect_group_state(&ctx, request.group_key.clone())
            .register_dispatch(EffectGroupRegisterDispatchRequest { addresses })
            .call()
            .await?
            .into_body();
        match registered {
            EffectGroupRegisterDispatchResponse::Registered
            | EffectGroupRegisterDispatchResponse::AlreadyRegistered
            | EffectGroupRegisterDispatchResponse::AlreadyClosed => {}
            EffectGroupRegisterDispatchResponse::Retired => return Ok(Reply::at(wire, ())),
            other => {
                return Err(TerminalError::new(format!(
                    "register dispatch protocol defect for {}: {other:?}",
                    request.group_key
                ))
                .into());
            }
        }

        // Holding the calls is the tracking. A child's outcome is not read
        // here and must not be: the child records its own settlement in the
        // index before it returns, so this dispatcher has nothing to add and
        // no authority to decide anything from what it sees.
        //
        // Failures are therefore absorbed rather than propagated, in a fixed
        // position order so the journal is replay-stable. A child cancelled by
        // `close` or `retire` completes its call with a terminal error, and a
        // child that hit a protocol defect has already failed its own
        // invocation; propagating either would fail this handler, and Restate
        // would retry the whole dispatch forever against a group that is
        // already correctly recorded. Worse, failing here would drop the
        // remaining children's tracking, which is the one thing this loop
        // exists to hold.
        for (position, call) in calls {
            if let Err(error) = call.await {
                tracing::debug!(
                    group_key = %request.group_key,
                    position,
                    %error,
                    "effect-group child call ended without a value; its settlement is the index's",
                );
            }
        }
        Ok(Reply::at(wire, ()))
    }

    async fn preflight(
        &self,
        _ctx: SharedWorkflowContext<'_>,
        call: Call<Vec<RuntimeEffectEnvelope>>,
    ) -> HandlerResult<Reply<Option<usize>>> {
        let (wire, children) = call.open()?;
        // This handler may run on any worker of the deployment. Preflight
        // checks registered routes before any generic child is dispatched.
        Ok(Reply::at(
            wire,
            children
                .iter()
                .position(|child| !self.executors.routes(child)),
        ))
    }

    async fn child(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<EffectGroupChildRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let result = self.run_child(ctx, &request).await;
        if result.is_ok() {
            self.executors.release_child(&request.envelope);
        }
        result.map(|()| Reply::at(wire, ()))
    }

    async fn retire(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<String>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, group_key) = call.open()?;
        let retired = self
            .route
            .namespace()
            .effect_group_state(&ctx, group_key.clone())
            .retire()
            .call()
            .await?
            .into_body();
        let cleanup = match retired {
            EffectGroupRetireResponse::Retired { cleanup }
            | EffectGroupRetireResponse::AlreadyRetired { cleanup } => cleanup,
            EffectGroupRetireResponse::Tombstone | EffectGroupRetireResponse::UnknownGroup => {
                self.executors.release_group(&group_key);
                return Ok(Reply::at(wire, ()));
            }
        };
        if let EffectGroupDispatchState::Adopted { id, .. } = &cleanup.dispatcher {
            ctx.invocation_handle(id.clone()).cancel();
            match ctx
                .invocation_handle(id.clone())
                .attach::<Reply<()>>()
                .await
            {
                Ok(_) | Err(_) => {}
            }
        }
        for invocation_id in cleanup.dispatched.values() {
            ctx.invocation_handle(invocation_id.clone()).cancel();
        }
        let cancelled = self
            .route
            .namespace()
            .effect_group_state(&ctx, group_key.clone())
            .retirement_cancel()
            .call()
            .await?
            .into_body();
        match cancelled {
            EffectGroupRetirementCancelResponse::Applied
            | EffectGroupRetirementCancelResponse::AlreadyApplied => {}
            other => {
                return Err(TerminalError::new(format!(
                    "effect group {group_key} retirement could not install canceller-side terminals: {other:?}"
                ))
                .into());
            }
        }
        for position in 0..cleanup.children() {
            self.route
                .namespace()
                .effect_group_payload(&ctx, payload_key(&group_key, position))
                .retire()
                .call()
                .await?;
        }
        // Every notice was answered `Retired` by the index's own `retire`,
        // and a later subscriber is answered from the retired record, so
        // nothing waits on the payload bytes this deletes (FIG-4344).
        for position in 0..cleanup.children() {
            self.route
                .namespace()
                .effect_group_payload(&ctx, payload_key(&group_key, position))
                .delete_bytes()
                .call()
                .await?;
        }
        let finished = self
            .route
            .namespace()
            .effect_group_state(&ctx, group_key.clone())
            .finish_retirement()
            .call()
            .await?
            .into_body();
        match finished {
            EffectGroupFinishRetirementResponse::Finished
            | EffectGroupFinishRetirementResponse::AlreadyFinished => {
                self.executors.release_group(&group_key);
                Ok(Reply::at(wire, ()))
            }
            other => Err(TerminalError::new(format!(
                "effect group {group_key} retirement could not reduce the index to its tombstone: {other:?}"
            ))
            .into()),
        }
    }
}

/// The FIG-3619 gate a child runs at invocation entry: the typed refusal
/// when the session that owns the child's scope is on a generation this build
/// does not admit, and `None` when it may run.
///
/// Only a session scope has an owning session to read. A process-scope or
/// runtime-operation child passes: a process's segment and program-identity
/// gates own its generation, and tying it to the session that started it
/// would refuse a live detached process whenever that session's generation
/// moved. The read is live on every attempt, before the invocation's first
/// journal entry, like the lease admission a parent turn runs on replay; a
/// marker never moves back to a refused generation, so every attempt takes
/// the same branch. A store or catalog failure is retried; a catalog with no
/// by-id lookup is a wiring fault no retry repairs.
async fn session_generation_refusal(
    sessions: &dyn lash_core::DeploymentStore,
    request: &EffectGroupChildRequest,
) -> HandlerResult<Option<RuntimeEffectControllerError>> {
    let Some(session_id) = request.envelope.invocation.execution_scope().session_id() else {
        return Ok(None);
    };
    match lash_core::admit_session_state_generation(sessions, session_id).await {
        Ok(()) => Ok(None),
        Err(
            refusal @ (lash_core::StoreError::SessionStateVersionUnsupported { .. }
            | lash_core::StoreError::SessionStateVersionNewerThanRuntime { .. }),
            // The settlement carries the code and a message naming both
            // generations: the opener reading it runs the build that wrote the
            // session, which decodes a code it does not know as a foreign
            // recorded outcome.
        ) => Ok(Some(RuntimeEffectControllerError::from(refusal))),
        Err(error @ lash_core::StoreError::UnsupportedStoreOperation { .. }) => {
            Err(TerminalError::new(format!(
                "effect group {} child {} cannot read its owning session `{session_id}`'s \
                 state generation: {error}",
                request.group_key, request.position
            ))
            .into())
        }
        Err(error) => Err(std::io::Error::other(format!(
            "read the state generation of session `{session_id}` owning effect group {} \
             child {}: {error}",
            request.group_key, request.position
        ))
        .into()),
    }
}

/// The typed refusal of a child whose lane is served by a deployment that can
/// never execute it (FIG-4550): terminal, and it names the capability.
fn unroutable_error(
    request: &EffectGroupChildRequest,
    missing: lash_core::GroupChildCapability,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::group_child_unroutable(
        missing,
        format!(
            "effect group {} child {} reached a deployment that serves its lane and has no \
             `{missing}`, so no attempt there can execute it; the child settles with this \
             refusal rather than retrying",
            request.group_key, request.position
        ),
    )
}

/// The §8 typed failure: this invocation is a successor minted under the
/// idempotency key after the retained child invocation's retention expired —
/// it never runs, and its settlement records the refusal so the opener's
/// rank wait resolves instead of stranding.
fn attach_expired_error(request: &EffectGroupChildRequest) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectGroupChildAttachExpired,
        format!(
            "effect group {} child {} attached an invocation id the index does not \
             retain; the retained invocation's retention expired, so the child \
             settles with this failure rather than re-running under a fresh \
             identity (ADR 0099 §8)",
            request.group_key, request.position
        ),
    )
}

/// Whether the engine retries a child that failed with `error` rather than
/// recording it: a live fault only (FIG-3575).
///
/// A live fault is a fact about this attempt, which a retry under a healthy
/// substrate repairs. A park is not: every run by this build refuses the same
/// replay again (FIG-3586), so an engine retry would spin forever. A parked
/// child records its refusal as its settlement, which hands the park to the
/// waiting turn, and the turn parks.
fn is_engine_retried_fault(error: &RuntimeEffectControllerError) -> bool {
    error.turn_failure_cause() == lash_core::TurnFailureCause::LiveFault
}

/// A child whose run ended in a live fault, or parked on a replay divergence,
/// fails this invocation with a retryable error instead of recording a
/// settlement.
///
/// The fault is a fact about this attempt, never the child's outcome: a
/// recorded `Failed` would replay it on every read of the group, so the turn
/// could only abort again. Failing retryably leaves the index untouched and
/// hands the child to the engine, which re-runs the invocation — replaying
/// every step it already journaled under the same keys — until it settles.
fn refuse_unrecorded_abort(
    request: &EffectGroupChildRequest,
    outcome: &EffectGroupChildRunOutcome,
) -> Result<(), restate_sdk::errors::HandlerError> {
    match outcome {
        EffectGroupChildRunOutcome::Completed {
            outcome: Err(error),
        } if is_engine_retried_fault(error) => {
            Err(crate::turn_handler::retried_attempt_failure(format!(
                "effect group {} child {} aborted with a live fault, which is never its \
             recorded outcome; the engine retries the child: {}",
                request.group_key,
                request.position,
                error.attempt_failure_text()
            )))
        }
        // A child whose replay diverged parks: it is found mid-replay, where
        // recording a settlement would propose a command the journal does not
        // hold, so the attempt ends the one way a park ends (FIG-3697).
        EffectGroupChildRunOutcome::Completed {
            outcome: Err(error),
        } if error.turn_failure_cause() == lash_core::TurnFailureCause::Parked => {
            Err(crate::parked_turn_failure(format!(
                "effect group {} child {}: {}",
                request.group_key,
                request.position,
                error.attempt_failure_text()
            )))
        }
        _ => Ok(()),
    }
}

/// A executed child's run outcome: the typed end of a child its cancel fact
/// ended is `Cancelled`, and anything else is what the shift produced.
fn child_run_outcome(
    executed: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
) -> EffectGroupChildRunOutcome {
    match executed {
        Err(error) if error.code == RuntimeErrorCode::RuntimeEffectGroupChildCancelled => {
            EffectGroupChildRunOutcome::Cancelled
        }
        outcome => EffectGroupChildRunOutcome::Completed { outcome },
    }
}

/// Records one child's terminal in the index, writing its payload first when
/// the outcome carries one.
///
/// `receipt` is the rank this invocation's own §4 answer reserved, when it
/// has one (FIG-4308): the shift's own commit, which already waited at the §5
/// barrier if the child had intents to drain, or the earlier commit whose
/// final this invocation drained. The seat publishes that rank without
/// re-reading the commit. The receipt is not proof of a drain; it only says
/// which rank this invocation's answer reserved. Without one, the outcome is
/// offered to the point first, and the committed final wins: a final an
/// earlier invocation committed is seated in place of this outcome.
async fn record_child_settlement(
    ctx: &SharedWorkflowContext<'_>,
    namespace: &crate::RestateNamespace,
    request: &EffectGroupChildRequest,
    outcome: EffectGroupChildRunOutcome,
    receipt: Option<u64>,
) -> HandlerResult<()> {
    if receipt.is_none() {
        match commit_child_final(ctx, namespace, request, EffectGroupCommittedFinal::Held).await? {
            PointAnswer::Won => {}
            PointAnswer::Retired => return Ok(()),
            PointAnswer::Taken { committed, .. } => {
                return seat_committed_final(
                    ctx,
                    namespace,
                    request,
                    committed,
                    "this invocation's own outcome reached the point after it",
                )
                .await;
            }
        }
    }
    seat_child_outcome(ctx, namespace, request, outcome).await
}

/// Seats one child's outcome at the rank its §4 decision reserved: its
/// payload first when the outcome carries one, then its settlement.
async fn seat_child_outcome(
    ctx: &SharedWorkflowContext<'_>,
    namespace: &crate::RestateNamespace,
    request: &EffectGroupChildRequest,
    outcome: EffectGroupChildRunOutcome,
) -> HandlerResult<()> {
    let terminal = match outcome {
        EffectGroupChildRunOutcome::Cancelled => EffectGroupSettlementTerminal::Cancelled,
        EffectGroupChildRunOutcome::Completed {
            outcome: Err(error),
        } => EffectGroupSettlementTerminal::Failed { error },
        EffectGroupChildRunOutcome::Completed {
            outcome: Ok(outcome),
        } => {
            let bytes = serde_json::to_vec(&outcome).map_err(|error| {
                TerminalError::new(format!(
                    "serialize effect group {} child {} outcome: {error}",
                    request.group_key, request.position
                ))
            })?;
            let put = namespace
                .effect_group_payload(ctx, payload_key(&request.group_key, request.position))
                .put(EffectGroupPayloadPutRequest { bytes })
                .call()
                .await?
                .into_body();
            match put {
                EffectGroupPayloadPutResponse::Written
                | EffectGroupPayloadPutResponse::Duplicate => {
                    EffectGroupSettlementTerminal::StoredPayload
                }
                EffectGroupPayloadPutResponse::Retired => return Ok(()),
                EffectGroupPayloadPutResponse::Conflict => {
                    return Err(TerminalError::new(format!(
                        "payload byte fence conflict for effect group {} child {}",
                        request.group_key, request.position
                    ))
                    .into());
                }
            }
        }
    };
    #[cfg(test)]
    crate::tests::effect_group_routing_miss::before_seat(&request.group_key);
    let recorded = namespace
        .effect_group_state(ctx, request.group_key.clone())
        .record_settlement(EffectGroupRecordSettlementRequest {
            position: request.position,
            terminal,
        })
        .call()
        .await?
        .into_body();
    #[cfg(test)]
    crate::tests::effect_group_routing_miss::after_seat(&request.group_key).await?;
    match recorded {
        EffectGroupRecordSettlementResponse::Recorded { .. }
        | EffectGroupRecordSettlementResponse::Duplicate { .. }
        | EffectGroupRecordSettlementResponse::Retired => Ok(()),
        other => Err(TerminalError::new(format!(
            "record settlement protocol defect for {} child {}: {other:?}",
            request.group_key, request.position
        ))
        .into()),
    }
}

/// Seats the final an earlier invocation committed, where this invocation
/// cannot drain it: the committed refusal as it was recorded, and otherwise
/// the typed report that the committed final is lost, with `why`. A committed
/// final seats nothing that drains, so the seat waits on no sibling.
async fn seat_committed_final(
    ctx: &SharedWorkflowContext<'_>,
    namespace: &crate::RestateNamespace,
    request: &EffectGroupChildRequest,
    committed: EffectGroupCommittedFinal,
    why: &str,
) -> HandlerResult<()> {
    let error = match committed {
        EffectGroupCommittedFinal::Refusal { error } => error,
        EffectGroupCommittedFinal::Held => committed_final_lost(
            request,
            &format!("{why}, and only the invocation that committed it held its outcome"),
        ),
    };
    seat_child_outcome(
        ctx,
        namespace,
        request,
        EffectGroupChildRunOutcome::Completed {
            outcome: Err(error),
        },
    )
    .await
}

/// The typed report of a committed final this invocation cannot realize: an
/// explicit terminal, never the refusal it would otherwise have seated.
fn committed_final_lost(
    request: &EffectGroupChildRequest,
    why: &str,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectGroupChildCommittedFinalLost,
        format!(
            "effect group {} child {}: an earlier invocation committed the child's final \
             at the §4 point and ended before its seat; {why} (ADR 0099 §5)",
            request.group_key, request.position
        ),
    )
}

/// What the §4 point answered a final this invocation offered it.
enum PointAnswer {
    /// This invocation's final holds the point, and its rank is reserved.
    Won,
    /// An earlier invocation's final holds the point at `rank`: it wins over
    /// the one offered, and this invocation seats it.
    Taken {
        committed: EffectGroupCommittedFinal,
    },
    /// The group retired meanwhile, and retirement already settled the child,
    /// as the payload and settlement writes treat the same answer.
    Retired,
}

/// The §4 boundary for a final this invocation offers from dispatch — an
/// atomic or wait child's outcome, or the refusal of a child this invocation cannot run: the index
/// decides the child's final before its payload and settlement exist. A final
/// the cancel disposition beat is refused by name, and its payload and
/// settlement never write.
///
/// A fresh commit drains nothing, so its seat waits on no one: its rank is
/// reserved here. Neither does a final an earlier invocation committed, when
/// this invocation seats it without draining it.
async fn commit_child_final(
    ctx: &SharedWorkflowContext<'_>,
    namespace: &crate::RestateNamespace,
    request: &EffectGroupChildRequest,
    offered: EffectGroupCommittedFinal,
) -> HandlerResult<PointAnswer> {
    let committed = namespace
        .effect_group_state(ctx, request.group_key.clone())
        .commit_child(EffectGroupCommitChildRequest {
            replay_key: request.envelope.invocation.effect_replay_key().to_string(),
            committed: offered,
        })
        .call()
        .await?
        .into_body();
    match committed {
        EffectGroupCommitChildResponse::Committed { .. } => Ok(PointAnswer::Won),
        EffectGroupCommitChildResponse::AlreadyCommitted { committed, .. } => {
            Ok(PointAnswer::Taken { committed })
        }
        EffectGroupCommitChildResponse::Retired => Ok(PointAnswer::Retired),
        EffectGroupCommitChildResponse::CancelDecided { .. } => {
            let refusal = RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided,
                format!(
                    "the final record of effect group {} child {} reached durable \
                         arbitration after its cancel disposition committed; the refusal \
                         is the whole record and no payload or settlement journals \
                         beneath it",
                    request.group_key, request.position
                ),
            );
            Err(TerminalError::new(refusal.to_record()).into())
        }
        other => Err(TerminalError::new(format!(
            "commit child protocol defect for {} child {}: {other:?}",
            request.group_key, request.position
        ))
        .into()),
    }
}

/// A child this invocation settles without running it.
enum UnrunChild {
    /// The session that owns the child's scope is on a state generation this
    /// build does not admit (FIG-3619): the typed refusal.
    GenerationRefused(RuntimeEffectControllerError),
    /// The index retains another invocation id for the child's position (§8).
    AttachExpired,
    /// The deployment that first took the child lacks a capability it needs
    /// (FIG-4550).
    Unroutable(lash_core::GroupChildCapability),
}

impl UnrunChild {
    /// The typed refusal this invocation offers the §4 point.
    fn refusal(&self, request: &EffectGroupChildRequest) -> RuntimeEffectControllerError {
        match self {
            Self::GenerationRefused(refusal) => refusal.clone(),
            Self::AttachExpired => attach_expired_error(request),
            Self::Unroutable(missing) => unroutable_error(request, *missing),
        }
    }

    /// Why this invocation cannot realize a final an earlier one committed.
    fn why_unrealized(&self) -> String {
        match self {
            Self::GenerationRefused(refusal) => format!(
                "this lane's build refuses a session state generation its opener's build \
                 admitted, which only a forged marker or operator-forced state produces \
                 ({refusal})"
            ),
            Self::AttachExpired => {
                "the invocation that committed it is gone, and its retention expired".to_owned()
            }
            Self::Unroutable(missing) => {
                format!("the deployment serving this lane has no `{missing}`")
            }
        }
    }
}

/// A wait child whose admission the index refused for a reason other than a
/// cancel decision (an unknown group, an unrecorded invocation, a refused
/// group) releases its own wait, since no deciding handler did (FIG-3567). A
/// child the close or the retirement decided is answered `CancelDecided` and
/// releases nothing: the deciding handler already did (FIG-3630). The release
/// is idempotent: a wait that already holds a terminal answers the same way
/// on a replay.
async fn release_unadmitted_wait(
    ctx: SharedWorkflowContext<'_>,
    authority_id: crate::ingress::RestateAuthorityId,
    build_generation: lash_core::engine::BuildGeneration,
    namespace: crate::RestateNamespace,
    request: &EffectGroupChildRequest,
) -> HandlerResult<()> {
    let RuntimeEffectCommand::AwaitEvent { key } = &request.envelope.command else {
        return Ok(());
    };
    let controller = RestateRuntimeEffectController::new(ctx, authority_id, build_generation)
        .in_namespace(namespace);
    lash_core::AwaitEventResolver::resolve_await_event(
        &controller,
        key,
        lash_core::Resolution::Cancelled,
    )
    .await
    .map_err(|error| {
        std::io::Error::other(format!(
            "release the refused wait child of effect group {} position {}: {error}",
            request.group_key, request.position
        ))
    })?;
    Ok(())
}
