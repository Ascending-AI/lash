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

#[derive(Clone)]
pub(crate) struct EffectGroupDispatchImpl {
    pub(super) executors: Arc<dyn GroupExecutors>,
    pub(super) ingress: RestateIngressClient,
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
        infinite_retry_policy: RunRetryPolicy,
        sessions: Arc<dyn lash_core::DeploymentStore>,
        route: crate::services::ServiceRoute,
        build_generation: lash_core::engine::BuildGeneration,
    ) -> Self {
        Self {
            executors: host.group_executors(),
            ingress,
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

impl EffectGroupDispatchImpl {
    /// Ends a tool child whose drive refused where it parks its opener,
    /// recording nothing (FIG-3725).
    ///
    /// - A group its opener closed or retired no longer needs the child: only
    ///   the child's own attempt ends, and no turn is parked for it.
    /// - Otherwise the child writes its turn's park and ends its attempt the
    ///   way a park ends (FIG-3697): every retry refuses and parks again
    ///   until the tool is restored or the turn is cancelled. A park the store
    ///   did not take fails the attempt as a live fault, so the retry writes
    ///   it again.
    /// - A child whose scope names no turn has no park to write, so it does
    ///   not retry unseen: a drift refused at the live frontier settles as the
    ///   child's typed outcome, which its opener reads (a process segment
    ///   fails its run). Any other refusal was found mid-replay, where no
    ///   settlement can be journaled, and ends the attempt as before.
    async fn end_parked_child<'ctx>(
        &self,
        ctx: &SharedWorkflowContext<'ctx>,
        request: &EffectGroupChildRequest,
        child: &lash_core::facade_support::ToolChildRequest,
        refusal: &RuntimeEffectControllerError,
        outcome: &EffectGroupChildRunOutcome,
    ) -> HandlerResult<()> {
        let label = format!(
            "effect group {} child {}",
            request.group_key, request.position
        );
        if self.opener_released(&request.group_key).await {
            return Err(crate::parked_turn_failure(format!(
                "{label}: its group is closed, so no turn is parked for it: {refusal}"
            )));
        }
        let parked = crate::turn_handler::park_refused_group_child(
            self.sessions.as_ref(),
            child.scope.admitted_scope.scope(),
            refusal,
        )
        .await;
        match parked {
            Ok(Some(_)) => Err(crate::parked_turn_failure(format!("{label}: {refusal}"))),
            Ok(None) if refusal.code == RuntimeErrorCode::LashlangCellBindingDrift => {
                tracing::warn!(
                    group_key = %request.group_key,
                    position = request.position,
                    %refusal,
                    "a drifted tool child whose scope names no turn settles its refusal"
                );
                record_child_settlement(ctx, self.route.namespace(), request, outcome.clone()).await
            }
            Ok(None) => Err(crate::parked_turn_failure(format!("{label}: {refusal}"))),
            Err(error) => Err(std::io::Error::other(format!(
                "{label} parked on `{}` and its turn's park could not be recorded: {error}",
                refusal.code
            ))
            .into()),
        }
    }

    /// Whether the opener of `group_key` no longer needs its children's
    /// ranks: the group is closed or retired. Read through ingress, outside
    /// the child's journal, since nothing may be journaled after a refused
    /// run's orphan; a read that fails answers `false`, so the park is kept.
    async fn opener_released(&self, group_key: &str) -> bool {
        matches!(
            self.ingress
                .call_lash_object::<_, EffectGroupProbeResponse>(
                    &self
                        .route
                        .namespace()
                        .stable(crate::LashService::EffectGroupState)
                        .name(),
                    group_key,
                    "probe",
                    &(),
                )
                .await,
            Ok(EffectGroupProbeResponse::Exists {
                phase: EffectGroupPhase::Closed | EffectGroupPhase::Retired,
                ..
            })
        )
    }
}

/// The effect-group dispatcher: sends a group's children and runs each one.
///
/// A pinned service (FIG-3795): each build binds it under its stable name
/// and under its generation's lane, and a group's opener records the lane
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
        // its effect can be dispatched. A session's state generation moves
        // independently of the build that opened the group, so a child can
        // still land on a session another build moved on. A refused child
        // settles with the typed refusal, which resolves the opener's rank
        // wait instead of stranding it; its effect never runs.
        if let Some(refusal) = session_generation_refusal(self.sessions.as_ref(), request).await? {
            return record_child_settlement(
                &ctx,
                self.route.namespace(),
                request,
                EffectGroupChildRunOutcome::Completed {
                    outcome: Err(refusal),
                },
            )
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
                // §8: the index retains a different invocation id for this
                // position — the original's retention expired and the
                // idempotency-keyed dispatch minted this successor. The child
                // settles with the typed failure rather than running under an
                // identity the group never recorded.
                return record_child_settlement(
                    &ctx,
                    self.route.namespace(),
                    request,
                    EffectGroupChildRunOutcome::Completed {
                        outcome: Err(attach_expired_error(request)),
                    },
                )
                .await;
            }
            EffectGroupAdmissionResponse::CancelDecided => {
                return Ok(());
            }
            EffectGroupAdmissionResponse::Refused => {
                release_unadmitted_wait(
                    ctx,
                    self.authority_id.clone(),
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
                let key = group_wait_key(
                    &request.shape.wait_scope,
                    &request.group_key,
                    EffectGroupWaitKind::Admit(request.position),
                )?;
                let replay_key = key.key_id.clone();
                let address = RestateDurableWaitAddress::for_key(&key);
                self.route
                    .namespace()
                    .durable_wait_workflow(&ctx, address.workflow_key)
                    .await_resolution(
                        RestateDurableWaitAwaitRequest {
                            key,
                            deadline: None,
                        }
                        .into(),
                    )
                    .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                    .call()
                    .await?;
                // ADMIT is notification only. Authorization always comes from
                // this one fresh, mapping-exact call after the wake.
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
                return record_child_settlement(
                    &ctx,
                    self.route.namespace(),
                    request,
                    EffectGroupChildRunOutcome::Completed {
                        outcome: Err(attach_expired_error(request)),
                    },
                )
                .await;
            }
            EffectGroupAdmissionResponse::CancelDecided => {
                return Ok(());
            }
            EffectGroupAdmissionResponse::Refused => {
                release_unadmitted_wait(
                    ctx,
                    self.authority_id.clone(),
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
        let child_replay_key = request.envelope.invocation.replay_key().to_string();
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

        let cancel_key = group_wait_key(
            &request.shape.wait_scope,
            &request.group_key,
            EffectGroupWaitKind::Cancel(request.shape.replay_key(request.position)?),
        )?;
        // The child's durable cancel fact (ADR 0105 §4, FIG-3904). No child
        // races it at handler level: a wait child races it as a journaled
        // arm, a tool child peeks it at its step boundaries and watches it
        // inside each attempt, and an atomic child watches it inside its
        // recorded body.
        let child_cancel = GroupChildCancel::new(
            self.ingress.clone(),
            self.route.namespace().clone(),
            cancel_key,
        );

        if let RuntimeEffectCommand::ToolInvocation { request: child } = &request.envelope.command {
            // ADR 0099 §2: a tool child is a handler-level invocation driver,
            // not a recorded body. Its replayable work — retries, deferred
            // completion, intent realization, completion-key derivation —
            // runs as journaled steps of *this* invocation; only the atomic
            // `ToolAttempt` executions it emits enter `ctx.run`. Resolving the
            // driver uses the same `GroupExecutors` answer first dispatch and
            // recovery both take (ADR 0065): there is no second route and no
            // caller closure.
            let Some(executor) = self.executors.executor_for(&request.envelope) else {
                return Err(std::io::Error::other(format!(
                    "no executor currently routes effect group {} tool child {}; retry on a carrying deployment",
                    request.group_key, request.position
                ))
                .into());
            };
            let Some(driver) = executor.tool_child_driver() else {
                return Err(TerminalError::new(format!(
                    "effect group {} tool child {} resolved to an executor with no handler-level driver",
                    request.group_key, request.position
                ))
                .into());
            };
            let controller = RestateRuntimeEffectController::new(ctx, self.authority_id.clone())
                .in_namespace(self.route.namespace().clone())
                .with_build_generation(self.build_generation.clone())
                .with_group_child_cancel(child_cancel);
            // The child's own admitted controller, bound to its recorded
            // identity: the recorded pair — claim scope and the incarnation
            // it was admitted under — never the dispatching scope and never
            // fresh admission (ADR 0099 §3), and every semantic effect it
            // serves is admitted through the index under the binding's child
            // (ADR 0099 §4, FIG-3470). A `ToolInvocation` that reached group
            // dispatch without retained membership is a shape error — never
            // run unbound.
            let Some(membership) = request.envelope.group.as_deref().cloned() else {
                return Err(TerminalError::new(format!(
                    "effect group {} tool child {} carries no retained membership; \
                     a child without one has no identity to bind a controller to",
                    request.group_key, request.position
                ))
                .into());
            };
            let binding = lash_core::GroupChildBinding {
                child: request.envelope.invocation.address.clone(),
                membership,
            };
            let scoped = controller
                .scoped_effect_controller_for_group_child(
                    child.scope.admitted_scope.clone(),
                    binding,
                )
                .map_err(TerminalError::from_error)?;
            // Routed through the host's stack before its first effect. A
            // failed route is the child's outcome, as any failure of its
            // drive is, so the opener's rank wait always learns of it.
            let routed = self.executors.route_handler_child_controller(scoped);
            let address = request.envelope.invocation.address.clone();
            // The drive runs to its own end: the child's cancel ends it at a
            // journaled peek, a journaled wait arm or an attempt's recorded
            // outcome, each of which a replay takes as the first execution
            // did, and never by dropping it mid-journal.
            let driven = match routed {
                Ok(scoped) => driver.drive(child, address, scoped).await,
                Err(error) => Err(lash_core::RuntimeEffectControllerError::from(error)),
            };
            let outcome = child_run_outcome(driven);
            // A child that parks settles nothing, so its opener's rank wait
            // cannot learn of it (FIG-3725).
            if let EffectGroupChildRunOutcome::Completed {
                outcome: Err(refusal),
            } = &outcome
                && refusal.turn_failure_cause() == lash_core::TurnFailureCause::Parked
            {
                return self
                    .end_parked_child(controller.context(), request, child, refusal, &outcome)
                    .await;
            }
            refuse_unrecorded_abort(request, &outcome)?;
            // The outcome is journaled once, as the execution that first
            // reached it built it. Its recorded steps replay the same, but
            // what the driver builds beside them does not have to: a child
            // whose opener was live lent its stream to that opener, while one
            // that ran on a pinned or deployment-built context carries a
            // recorded stream. A replay finds its context wherever it can, so
            // it settles the recorded value rather than its own (FIG-3985).
            let Json(outcome) =
                controller
                    .context()
                    .run(move || async move {
                        Ok::<_, restate_sdk::errors::HandlerError>(Json(outcome))
                    })
                    .name(format!(
                        "lash:effect-group:settled:{}:{}",
                        request.group_key, request.position
                    ))
                    .await?;
            return record_child_settlement(
                controller.context(),
                self.route.namespace(),
                request,
                outcome,
            )
            .await;
        }

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
                return Err(std::io::Error::other(format!(
                    "no executor currently routes effect group {} child {}; retry on a carrying deployment",
                    request.group_key, request.position
                ))
                .into());
            };
            let controller = RestateRuntimeEffectController::new(ctx, self.authority_id.clone())
                .in_namespace(self.route.namespace().clone())
                .with_build_generation(self.build_generation.clone())
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
            )
            .await;
        }

        let cancellation = tokio_util::sync::CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let envelope = request.envelope.clone();
        let executors = Arc::clone(&self.executors);
        let group_key = request.group_key.clone();
        let position = request.position;
        let mut run = Box::pin(
            ctx.run(move || async move {
                let Some(executor) = executors.executor_for(&envelope) else {
                    return Err(std::io::Error::other(format!(
                        "no executor currently routes effect group {group_key} child {position}; retry on a carrying deployment"
                    ))
                    .into());
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
                Ok(Json(outcome))
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

        record_child_settlement(&ctx, self.route.namespace(), request, outcome).await
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
        // Eager is load-bearing. `register_children` below is what resolves
        // READY and releases the opener, so awaiting any child before that
        // point would deadlock the open. Issue all, record all, register, then
        // hold.
        //
        // Every call is issued before any invocation id is awaited, and the
        // ids are recorded in one `record_dispatch` (FIG-4088). The engine
        // mints a call's id when it appends the call, so by the time the first
        // await suspends every id is journaled: the dispatch costs a fixed
        // number of suspensions whatever the width. Awaiting each id and
        // recording each dispatch in turn cost two per child, and on a
        // resumption that replays the journal, child i started only after
        // about 2i replays of a journal that grows with the width.
        let mut calls = Vec::with_capacity(children.len());
        for (position, envelope) in children.into_iter().enumerate() {
            let replay_key = shape.replay_key(position)?.to_string();
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
        let recorded = self
            .route
            .namespace()
            .effect_group_state(&ctx, request.group_key.clone())
            .record_dispatch(EffectGroupRecordDispatchRequest {
                dispatched: addresses.clone(),
            })
            .call()
            .await?
            .into_body();
        match recorded {
            EffectGroupRecordDispatchResponse::Recorded
            | EffectGroupRecordDispatchResponse::Duplicate => {}
            EffectGroupRecordDispatchResponse::Retired => return Ok(Reply::at(wire, ())),
            other => {
                return Err(TerminalError::new(format!(
                    "record dispatch protocol defect for {}: {other:?}",
                    request.group_key
                ))
                .into());
            }
        }
        let registered = self
            .route
            .namespace()
            .effect_group_state(&ctx, request.group_key.clone())
            .register_children(EffectGroupRegisterRequest { addresses })
            .call()
            .await?
            .into_body();
        match registered {
            EffectGroupRegisterResponse::Registered
            | EffectGroupRegisterResponse::AlreadyRegistered
            | EffectGroupRegisterResponse::AlreadyClosed
            | EffectGroupRegisterResponse::Retired => {}
            other => {
                return Err(TerminalError::new(format!(
                    "register children protocol defect for {}: {other:?}",
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
        // Routability, not a local executor: this handler may run on any
        // worker of the deployment, and a tool child whose opener is live on
        // another worker is still routed — it runs there (FIG-3630).
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
        // Wait retirement is retained in the durable-wait index. This shared
        // handler cannot mutate the index object directly without journaling
        // the calls, so resolve every one before deleting payload bytes.
        for (kind, resolution) in std::iter::once((
            EffectGroupWaitKind::Ready,
            EffectGroupWaitResolution::Retired,
        ))
        .chain((1..=cleanup.children() as u64).map(|rank| {
            (
                EffectGroupWaitKind::Rank(rank),
                EffectGroupWaitResolution::Retired,
            )
        }))
        .chain(cleanup.replay_keys.iter().map(|replay_key| {
            (
                EffectGroupWaitKind::Cancel(replay_key),
                EffectGroupWaitResolution::Retired,
            )
        }))
        .chain((0..cleanup.children()).map(|position| {
            (
                EffectGroupWaitKind::Admit(position),
                EffectGroupWaitResolution::Retired,
            )
        }))
        // A committed child that retirement cancelled before it seated never
        // resolves its own drained wake; every sibling parked behind it at
        // the §5 barrier — a dispatch workflow's settlement or a tool child's
        // intent drain — is released here instead of stranding.
        .chain((0..cleanup.children()).map(|position| {
            (
                EffectGroupWaitKind::Drained(position),
                EffectGroupWaitResolution::Retired,
            )
        })) {
            let key = group_wait_key(&cleanup.wait_scope, &group_key, kind)?;
            let replay_key = key.key_id.clone();
            let address = RestateDurableWaitAddress::for_key(&key);
            self.route
                .namespace()
                .durable_wait_registry(&ctx, durable_wait_index_object_key(&address))
                .retain_resolution(RestateDurableWaitResolveRequest {
                    key,
                    resolution: wait_resolution(resolution)?,
                })
                .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                .call()
                .await?;
        }
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
        } if is_engine_retried_fault(error) => Err(std::io::Error::other(format!(
            "effect group {} child {} aborted with a live fault, which is never its \
             recorded outcome; the engine retries the child: {error}",
            request.group_key, request.position
        ))
        .into()),
        // A child whose replay diverged parks: it is found mid-replay, where
        // recording a settlement would propose a command the journal does not
        // hold, so the attempt ends the one way a park ends (FIG-3697).
        EffectGroupChildRunOutcome::Completed {
            outcome: Err(error),
        } if error.turn_failure_cause() == lash_core::TurnFailureCause::Parked => {
            Err(crate::parked_turn_failure(format!(
                "effect group {} child {}: {error}",
                request.group_key, request.position
            )))
        }
        _ => Ok(()),
    }
}

/// A driven child's run outcome: the typed end of a child its cancel fact
/// ended is `Cancelled`, and anything else is what the drive produced.
fn child_run_outcome(
    driven: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
) -> EffectGroupChildRunOutcome {
    match driven {
        Err(error) if error.code == RuntimeErrorCode::RuntimeEffectGroupChildCancelled => {
            EffectGroupChildRunOutcome::Cancelled
        }
        outcome => EffectGroupChildRunOutcome::Completed { outcome },
    }
}

/// Records one child's terminal in the index, writing its payload first when
/// the outcome carries one.
///
/// The context is a parameter because the two callers hold it differently: an
/// atomic child's `ctx` is still free after its `ctx.run` completes, while a
/// tool child's `ctx` lives inside the runtime controller the driver was
/// bound to and comes back through `context()` once the drive is over. The
/// protocol is identical either way — and a `Cancelled` terminal deliberately
/// writes no payload: settlement is the index-serialized arbitration point,
/// so a loser cancelled before its outcome landed leaves nothing for the
/// group to read.
async fn record_child_settlement(
    ctx: &SharedWorkflowContext<'_>,
    namespace: &crate::RestateNamespace,
    request: &EffectGroupChildRequest,
    outcome: EffectGroupChildRunOutcome,
) -> HandlerResult<()> {
    // The §4 boundary: the index decides this child's final before its
    // payload and settlement exist. A child whose own settle already
    // committed reads `AlreadyCommitted` back with the same position; one
    // the cancel disposition beat is refused by name, and its payload
    // and settlement never write.
    let committed = namespace
        .effect_group_state(ctx, request.group_key.clone())
        .commit_child(EffectGroupCommitChildRequest {
            replay_key: request.envelope.invocation.replay_key().to_string(),
        })
        .call()
        .await?
        .into_body();
    let blocking_positions = match committed {
        EffectGroupCommitChildResponse::Committed {
            blocking_positions, ..
        }
        | EffectGroupCommitChildResponse::AlreadyCommitted {
            blocking_positions, ..
        } => blocking_positions,
        // A child that settles without admission (a generation refusal, an
        // expired attach) can meet a group retired meanwhile; retirement
        // already settled it, as the payload and settlement writes below
        // treat the same answer.
        EffectGroupCommitChildResponse::Retired => return Ok(()),
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
            return Err(TerminalError::new(
                serde_json::to_string(&refusal).unwrap_or(refusal.message),
            )
            .into());
        }
        other => {
            return Err(TerminalError::new(format!(
                "commit child protocol defect for {} child {}: {other:?}",
                request.group_key, request.position
            ))
            .into());
        }
    };
    // The §5 barrier: committed siblings below this child seat their
    // settlements first. The last blocker's wake is durable, so a redrive
    // of this handler re-reads the index's answer rather than racing it.
    for position in blocking_positions {
        let key = group_wait_key(
            &request.shape.wait_scope,
            &request.group_key,
            EffectGroupWaitKind::Drained(position),
        )?;
        let replay_key = key.key_id.clone();
        let address = RestateDurableWaitAddress::for_key(&key);
        namespace
            .durable_wait_workflow(ctx, address.workflow_key)
            .await_resolution(
                RestateDurableWaitAwaitRequest {
                    key,
                    deadline: None,
                }
                .into(),
            )
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
            .call()
            .await?;
    }

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
    let recorded = namespace
        .effect_group_state(ctx, request.group_key.clone())
        .record_settlement(EffectGroupRecordSettlementRequest {
            position: request.position,
            terminal,
        })
        .call()
        .await?
        .into_body();
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
    namespace: crate::RestateNamespace,
    request: &EffectGroupChildRequest,
) -> HandlerResult<()> {
    let RuntimeEffectCommand::AwaitEvent { key } = &request.envelope.command else {
        return Ok(());
    };
    let controller = RestateRuntimeEffectController::new(ctx, authority_id).in_namespace(namespace);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> EffectGroupChildRequest {
        EffectGroupChildRequest {
            group_key: "group-1".to_owned(),
            shape: EffectGroupShape {
                wake: lash_core::GroupWakePolicy::All,
                loser_disposition: LoserPolicy::RunToCompletion,
                replay_keys: vec!["child-0".to_owned()],
                wait_scope: ExecutionScope::runtime_operation("group"),
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
            position: 0,
            envelope: RuntimeEffectEnvelope::new(
                lash_core::RuntimeEffectInvocation::new(
                    lash_core::EffectAddress::new(
                        ExecutionScope::runtime_operation("group"),
                        "group-1:child:0",
                    )
                    .expect("valid child address"),
                    lash_core::RuntimeAttribution::none(),
                    "effect",
                ),
                lash_core::RuntimeEffectCommand::LanguageRuntimeValue {
                    operation: "child".to_owned(),
                },
            ),
        }
    }

    /// The §8 typed failure is terminal and names the group and position the
    /// stranded rank wait needs: distinct from an ordinary refusal so the
    /// opener reads "the retained invocation expired", not "disallowed".
    #[test]
    fn attach_expired_error_is_the_typed_attach_expiry() {
        let error = attach_expired_error(&request());
        assert_eq!(
            error.code,
            RuntimeErrorCode::RuntimeEffectGroupChildAttachExpired
        );
        let message = error.to_string();
        assert!(message.contains("group-1"), "{message}");
    }
}
