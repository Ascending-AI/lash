//! Effect-group formation and settlement consumption for tool aggregates
//! (ADR 0099, FIG-3397).
//!
//! Every product tool batch and every Lashlang aggregate opens one durable
//! [`crate::RuntimeEffectGroup`]: a [`crate::RuntimeEffectCommand::ToolInvocation`]
//! child per unique tool call, and a `Sleep` child per timer whose deadline
//! was recorded at admission (§11 clause 4). The consumer observes durable
//! rank — the order children's final records committed (§5) — so a held
//! source-first leaf never blocks a later sibling's terminal.
//!
//! A consumer stops where its aggregate is decided (§10): `race` at the first
//! settlement, `any` at the first fulfilment, `all` at the first rejection,
//! `allSettled` and every all-results batch at exhaustion. A group decided
//! early is handed to the opener with the consumer's cursor; its losers keep
//! running, and the opener's end closes it (`opener_groups.rs`, §7). A
//! cancelled await closes its group under `Cancel` through the FIG-3410
//! closing driver.
//!
//! Settlement facts are incorporated through FIG-3411's
//! `incorporate_group_prefix`: the consumer journals the consumed prefix as
//! an `IncorporateGroupSettlements` record and applies each rank's recorded
//! facts once, while presentation reads the model return the child projected
//! and recorded rather than re-projecting it (§6).

use super::*;

use crate::runtime::effect::{
    GroupWakePolicy, LoserPolicy, ToolChildAdmission, ToolChildCompletionRouting, ToolChildRequest,
    ToolChildScope,
};

/// One prepared leaf of a tool-child group: everything formation needs to
/// mint the child's retained [`ToolChildRequest`] and the consumer needs to
/// apply its settlement.
///
/// `input_index` is the leaf's position in the caller's leaf vector; the
/// position inside the group's `children` is the leaf's index in the vector
/// passed to [`RuntimeExecutionContext::open_tool_child_group`]. Duplicate
/// operands never reach here: the caller deduplicates, and positions map onto
/// unique leaves above the group (ADR 0099 §10 L4).
pub(crate) struct PreparedToolChildLeaf {
    /// This leaf's index in the caller's input vector.
    pub input_index: usize,
    /// The prepared call plus its byte-identical `child:{index}:{call_id}`
    /// replay suffix, produced by [`crate::PreparedToolBatch::new_with_grants`];
    /// the suffix is attempt-identity material, so it is part of every attempt
    /// envelope's hash (ADR 0099 §3).
    pub call: crate::PreparedToolBatchCall,
    /// The authority the child was admitted under, pinned at formation so a
    /// reopen never re-reads the live catalog.
    pub admission: ToolChildAdmission,
    // Deliberately no `trace_hook`: `ToolChildExecutionTraceHook` is a live
    // callback and cannot ride a retained, journaled `ToolChildRequest`, so
    // group children run with no hook (the driver passes `None`).
}

/// One child of an aggregate's group, in group-position order.
pub(crate) enum PreparedGroupChild {
    /// A tool call, run by the invocation driver.
    Tool(Box<PreparedToolChildLeaf>),
    /// A timer from an unawaited `sleep(ms)`: a `Sleep` child whose deadline
    /// was recorded once, at admission (ADR 0099 §11 clause 4).
    Timer { deadline_ms: u64 },
}

impl PreparedGroupChild {
    /// The tool leaf, when this child is one.
    pub(crate) fn tool(&self) -> Option<&PreparedToolChildLeaf> {
        match self {
            Self::Tool(leaf) => Some(leaf),
            Self::Timer { .. } => None,
        }
    }
}

/// How a group's consumer decides its aggregate (ADR 0099 §10 L1). A
/// caller-side loop decision, never journaled; [`Self::wake`] is the journaled
/// wake policy it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolAggregateConsumer {
    /// Every settlement: `allSettled` and every all-results batch.
    AllSettled,
    /// Stop at the first consumed rejection: `Promise.all`.
    All,
    /// Stop at the first settlement: `Promise.race`.
    Race,
    /// Stop at the first fulfilment: `Promise.any`.
    Any,
}

impl ToolAggregateConsumer {
    /// The three-way journaled wake policy this four-way consumer mode folds
    /// into every child's envelope (ADR 0099 §10 L1, ADR 0065).
    #[must_use]
    pub fn wake(self) -> GroupWakePolicy {
        match self {
            Self::AllSettled | Self::All => GroupWakePolicy::All,
            Self::Race => GroupWakePolicy::First,
            Self::Any => GroupWakePolicy::FirstSuccess,
        }
    }

    /// Whether a settlement with this fulfilment decides the aggregate.
    #[must_use]
    pub fn decides(self, fulfilled: bool) -> bool {
        match self {
            Self::AllSettled => false,
            Self::All => !fulfilled,
            Self::Race => true,
            Self::Any => fulfilled,
        }
    }
}

/// One settled child as its consumer saw it.
pub(crate) enum GroupChildSettled {
    /// A tool child's completed call.
    Tool(Box<CompletedProtocolToolCall>),
    /// A timer child that elapsed; its fulfilment value is `undefined`.
    Timer,
}

impl GroupChildSettled {
    /// A timer always fulfils; a tool child fulfils when its output succeeded.
    pub(crate) fn fulfilled(&self) -> bool {
        match self {
            Self::Tool(completed) => {
                matches!(
                    completed.completed.output.outcome,
                    crate::ToolCallOutcome::Success(_)
                )
            }
            Self::Timer => true,
        }
    }
}

/// What [`RuntimeExecutionContext::consume_tool_child_group`] hands back.
///
/// `settled` is indexed by group position; `settlement_positions` is the
/// rank-ordered list of positions the group settled, which the batch surface
/// maps back to input indices for `ToolBatchReplies::settlement_order`.
pub(crate) struct ToolChildGroupSettled {
    /// One slot per group position. After exhaustion every slot is filled;
    /// after a cancelled await, unconsumed tool positions carry the cancelled
    /// completion; after a decision, only the consumed prefix is filled — a
    /// loser's value is never synthesized (§10 L6).
    pub settled: Vec<Option<GroupChildSettled>>,
    /// Positions in the order the group settled them (durable commit order,
    /// ADR 0099 §5), with cancel-unconsumed positions appended in position
    /// order so `validate_batch_settlement_order` still sees a permutation.
    pub settlement_positions: Vec<usize>,
    /// The position whose settlement decided the aggregate. When the group
    /// was not yet exhausted at that settlement it is the opener's: its losers
    /// keep running and the opener's end closes it.
    pub decided: Option<usize>,
    /// The await was cancelled with the turn.
    pub cancelled: bool,
}

impl RuntimeExecutionContext<'_> {
    /// The group key one batch's children are journaled under:
    /// `{scope_id}:group:{batch_id}` for a top-level batch and
    /// `{scope_id}:group:{parent_effect_id}:{batch_id}` for a nested one,
    /// mirroring how [`Self::tool_batch_invocation`] prefixes a nested batch.
    ///
    /// Host-code batches only: a language runtime's aggregate is keyed by its
    /// command key instead (FIG-3586).
    pub(crate) fn tool_child_group_key(&self, batch_id: &str) -> String {
        match self
            .parent_invocation
            .as_ref()
            .and_then(|parent| parent.effect_id())
        {
            Some(parent_effect_id) => format!(
                "{}:group:{parent_effect_id}:{batch_id}",
                self.execution_scope_id()
            ),
            None => format!("{}:group:{batch_id}", self.execution_scope_id()),
        }
    }

    /// Opens one durable effect group of tool children and returns its
    /// consumption handle (ADR 0099 §3).
    ///
    /// Formation retains, per child, the complete [`ToolChildRequest`] the
    /// journal needs to reconstruct and re-drive the child with no opener in
    /// scope: the pinned admission, the batch-derived attempt identity (so
    /// attempt envelopes hash identically to the pre-group batch path), the
    /// checked opener/scope pair, the recorded cancellation authority, the
    /// execution-environment reference published under
    /// `ArtifactOwner::Execution`, and the completion routing the admission
    /// computed.
    ///
    /// Every refusal is typed and happens before `open_effect_group` is
    /// called, so a refused formation leaves no group row stranded: an
    /// administrative scope with no derivable opener, an environment publish
    /// that refuses a retired owner, and a controller that names no
    /// await-event authority all return
    /// [`crate::RuntimeErrorCode::RuntimeEffectGroupShape`].
    ///
    /// Completion routing mirrors what `coordinate_tool_invocation` re-derives
    /// per attempt (attempt_coordinator.rs): `may_defer` is asked of the same
    /// `attempt_may_defer` seam, and the answer is *recorded* — a `NotNeeded`
    /// or `Unsupported` preparation records `Inline`, which forces the child's
    /// `may_defer` false so its attempt fails on a defer exactly as an
    /// `Unsupported` key does today; an `Issued` preparation records
    /// `Durable`. The attempt-side honour check
    /// refuses a recorded arm the controller would not have issued, so a
    /// replayed formation under a different deployment cannot silently
    /// reinterpret the routing.
    pub(crate) async fn open_tool_child_group(
        &self,
        group_invocation: crate::RuntimeEffectInvocation,
        group_key: String,
        batch_id: &str,
        children: &[PreparedGroupChild],
        wake: GroupWakePolicy,
        reopen: crate::GroupReopen,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        let scoped = self.dispatch.effect_controller.scoped();
        // Opening a group writes the journal; a replayed language command's
        // guard admits it or refuses it before any row exists (FIG-3586).
        scoped.admit_journal_write()?;
        let scope = scoped.execution_scope().clone();
        let admitted = scoped.admitted_scope().clone();
        let controller = self.dispatch.effect_controller.controller();

        let opener = crate::EffectOpener::for_scope(&admitted).map_err(|error| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                format!(
                    "a tool-child group cannot form under this scope: {error}; \
                     administrative scopes name no opener, and a child without \
                     an opener cannot be routed (ADR 0099 §1)"
                ),
            )
        })?;

        let authority = controller
            .await_event_authority_binding_id()
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    "a tool-child group requires the controller to name its await-event \
                     authority; without it the child records no cancellation authority it \
                     can honour",
                )
            })?;
        let binding_id = crate::runtime::turn_control_binding_id_for_scope(&authority, &scope)
            .map_err(crate::RuntimeEffectControllerError::from)?;
        let cancellation_authority =
            crate::TurnControlBindingId::new(binding_id).map_err(|error| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!("the derived cancellation authority is not a binding: {error}"),
                )
            })?;

        // The environment reference is a durable-owner publish retained inside
        // the request (ADR 0099 §3): a refusal here — a retired owner — is a
        // formation failure, never a silently absent environment.
        let execution_env = self
            .captured_process_execution_env_ref(&crate::ArtifactOwner::execution(scope.clone()))
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!("tool-child group formation could not publish its execution environment: {error}"),
                )
            })?;

        // Live trace/activity is the opener's (ToolSettlement plan rule 3):
        // started events are emitted here, at formation, exactly as the batch
        // path emitted them at each child's dispatch.
        for leaf in children.iter().filter_map(PreparedGroupChild::tool) {
            let call_id = leaf.call.call.call_id.clone();
            self.emit_tool_call_started(
                &call_id,
                &leaf.call.call.tool_name,
                leaf.call.call.args.clone(),
                tool_activity_id(&call_id),
            );
        }

        // The group's unique children are reserved against the opener's bound
        // before anything is journaled or dispatched (ADR 0099 §9).
        self.reserve_group_work(&group_key, children.len()).await?;
        let session_facts = self.tool_child_session_facts();
        let mut envelopes = Vec::with_capacity(children.len());
        for (position, child) in children.iter().enumerate() {
            let leaf = match child {
                PreparedGroupChild::Tool(leaf) => leaf,
                PreparedGroupChild::Timer { deadline_ms } => {
                    // A timer child carries the deadline recorded at
                    // admission, never a duration: a redrive, a reattachment
                    // and a duplicate position all wait on the same instant
                    // (ADR 0099 §11 clause 4).
                    envelopes.push(crate::RuntimeEffectEnvelope::new(
                        crate::RuntimeEffectInvocation::new(
                            crate::EffectAddress::new(
                                scope.clone(),
                                format!("{group_key}:child:{position}"),
                            )?,
                            self.effect_attribution(),
                            format!("tool-batch:{batch_id}:child:{position}"),
                        ),
                        crate::RuntimeEffectCommand::Sleep {
                            spec: crate::SleepSpec::Until {
                                deadline_ms: *deadline_ms,
                            },
                        },
                    ));
                    continue;
                }
            };
            let call_id = leaf.call.call.call_id.clone();
            let completion_routing = self
                .tool_child_completion_routing(
                    controller,
                    &scope,
                    &leaf.call.call.tool_id,
                    leaf.admission.grant(),
                    &call_id,
                )
                .await?;
            let mut request = ToolChildRequest::new(
                leaf.call.call.clone(),
                leaf.admission.clone(),
                crate::tool_dispatch::ToolAttemptEffectIdentity::Batch {
                    parent: group_invocation.clone().into_runtime_invocation(),
                    replay_suffix: leaf.call.replay_suffix.clone(),
                },
                ToolChildScope {
                    opener: opener.clone(),
                    admitted_scope: admitted.clone(),
                    session_id: self.dispatch.session_id.clone(),
                    agent_frame_id: self.dispatch.agent_frame_id.clone(),
                },
                cancellation_authority.clone(),
                execution_env.clone(),
                completion_routing,
                session_facts.clone(),
            );
            if let Some(process_ref) = opener.process_ref() {
                request = request.with_enclosing_process(process_ref.clone());
            }
            envelopes.push(crate::RuntimeEffectEnvelope::new(
                crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(
                        scope.clone(),
                        format!("{group_key}:child:{position}"),
                    )?,
                    self.effect_attribution(),
                    format!("tool-batch:{batch_id}:child:{position}"),
                ),
                crate::RuntimeEffectCommand::ToolInvocation {
                    request: Box::new(request),
                },
            ));
        }
        // Declared `RunToCompletion`: selection cancels nothing, and a loser
        // runs while its opener lives (§0 *live*). The opener's end narrows
        // the close to `Cancel` (§7).
        let group = crate::RuntimeEffectGroup::try_new(
            group_invocation,
            group_key.clone(),
            envelopes,
            wake,
            LoserPolicy::RunToCompletion,
        )
        .map(|group| group.with_reopen(reopen))
        .inspect_err(|_| self.release_group_work(&group_key))?;
        controller
            .open_effect_group(group)
            .await
            .inspect_err(|_| self.release_group_work(&group_key))
    }

    /// Records one leaf's completion routing from the same two admission facts
    /// the attempt coordinator consults: whether the tool may defer, and what
    /// the controller can issue for a completion key.
    async fn tool_child_completion_routing(
        &self,
        controller: &dyn crate::RuntimeEffectController,
        scope: &crate::ExecutionScope,
        tool_id: &crate::ToolId,
        grant: Option<&crate::ToolExecutionGrant>,
        call_id: &str,
    ) -> Result<ToolChildCompletionRouting, crate::RuntimeEffectControllerError> {
        if !self.dispatch.attempt_may_defer(tool_id, grant) {
            return Ok(ToolChildCompletionRouting::Inline);
        }
        match controller
            .prepare_completion_key(
                scope,
                crate::AwaitEventWaitIdentity::tool_completion(call_id),
                true,
            )
            .await
            .map_err(crate::RuntimeEffectControllerError::from)?
        {
            crate::CompletionKeyPreparation::NotNeeded
            | crate::CompletionKeyPreparation::Unsupported => {
                // `Inline` forces the child's `may_defer` false; its attempt
                // then fails on a defer exactly as an `Unsupported` key makes
                // it fail on the live path today. On a controller that answers
                // `Unsupported` for a deferable leaf the attempt-side honour
                // check surfaces `ControllerAborted`, which the consumer treats
                // as infrastructure — the batch surface's host-control channel.
                Ok(ToolChildCompletionRouting::Inline)
            }
            crate::CompletionKeyPreparation::Issued(_) => Ok(ToolChildCompletionRouting::Durable),
        }
    }

    /// Consumes a group's settlement order until its aggregate is decided or
    /// the group is exhausted (ADR 0099 §5, §10).
    ///
    /// Settlements are served by durable rank, so `settled[position]` fills in
    /// commit order — not input order. A `GroupSettlement.outcome: Err` is
    /// infrastructure, never a tool rejection (§10 L3: tool rejections arrive
    /// as ordinary `ToolCallOutput`s inside the outcome), and aborts the
    /// consume with the controller error; the group is handed to the opener
    /// first, so the opener's end closes it rather than leaving it live.
    ///
    /// **Decided** (`consumer.decides` on a settlement before exhaustion): the
    /// consumed prefix is incorporated and the group — with its cursor — is
    /// handed to the opener. Its losers keep running: selection cancels
    /// nothing (§0 *live*), and the opener's end closes it (§7).
    ///
    /// **Cancelled** (`RuntimeEffectGroupAwaitCancelled`): the turn was
    /// cancelled. Consumption stops, every unsettled tool position is filled
    /// with the batch surface's cancelled reply and appended to the settlement
    /// order in position order, and the group is closed under
    /// [`LoserPolicy::Cancel`]: the close records `closing` through the
    /// FIG-3410 driver, seats a cancelled terminal for every undecided child
    /// and fires the group's token, so a child that ignores cooperative
    /// cancellation is dropped as the batch path's cancel grace dropped it.
    /// The group — with the cursor after its incorporated prefix — is then
    /// handed to the opener, whose end incorporates the ranks that land after
    /// the close.
    ///
    /// **Exhausted**: the group is closed under the declared `RunToCompletion`
    /// disposition to release consumer interest: the close CASes the journaled
    /// `Live → Closing` fact and returns without waiting — finalization is
    /// host-owned and cursor-resumable. A close error is logged rather than
    /// failing the batch, because close is idempotent and retryable by the
    /// trait contract, the settlements are already consumed, and the opener's
    /// end resumes whatever closing is recorded under its scope.
    pub(crate) async fn consume_tool_child_group(
        &self,
        mut handle: crate::EffectGroupHandle,
        children: &[PreparedGroupChild],
        consumer: ToolAggregateConsumer,
    ) -> Result<ToolChildGroupSettled, crate::RuntimeEffectControllerError> {
        let controller = self.dispatch.effect_controller.controller();
        let cancel = self.cancellation_token.clone().unwrap_or_default();
        let mut settled: Vec<Option<GroupChildSettled>> =
            (0..children.len()).map(|_| None).collect();
        let mut settlement_positions = Vec::with_capacity(children.len());
        let mut decided = None;
        let mut lost_to_turn_gate = false;
        while !handle.is_exhausted() {
            // A turn that already recorded its cancellation awaits no rank:
            // the recorded fact answers exactly as a wait that lost to the
            // gate would, at the same point on every replay (FIG-3672 P9).
            let awaited = if self.is_cancelled() {
                Err(crate::runtime::effect::await_cancelled_error(
                    handle.group_key(),
                    handle.consumed() + 1,
                ))
            } else {
                let turn_cancel = self.turn_cancel_wait(cancel.child_token());
                let awaited = controller
                    .await_next_settlement(&mut handle, turn_cancel.clone())
                    .await;
                // Decided by the wait's recorded outcome alone, never by a
                // live read of the execution's token: a turn-observing rank
                // wait that ended cancelled is the turn's cancellation, and an
                // execution whose own stop ended it is cancelled either way.
                if matches!(&awaited, Err(error)
                    if error.code == crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled)
                    && turn_cancel.observes_turn_cancel()
                {
                    lost_to_turn_gate = true;
                }
                awaited
            };
            let settlement = match awaited {
                Ok(settlement) => settlement,
                Err(error)
                    if error.code == crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled =>
                {
                    for (position, child) in children.iter().enumerate() {
                        if settled[position].is_none()
                            && let PreparedGroupChild::Tool(leaf) = child
                        {
                            settled[position] = Some(GroupChildSettled::Tool(Box::new(
                                cancelled_group_leaf(leaf),
                            )));
                            settlement_positions.push(position);
                        }
                    }
                    // Incorporate the consumed prefix before abandoning the
                    // await: settled ranks are durable facts and the journaled
                    // `IncorporateGroupSettlements` record names them so a
                    // replay applies the same prefix (FIG-3411 part 2).
                    self.incorporate_group_prefix(&handle).await?;
                    // A cancelled turn no longer wants its unsettled children:
                    // close under `Cancel`, which records `closing`, seats a
                    // cancelled terminal for every undecided child and fires
                    // the group's token, so a child that ignores cooperative
                    // cancellation is dropped rather than left running under
                    // the turn's sessions — the batch path's cancel-grace
                    // observable. A committed child keeps its authority to
                    // finish its drain (§4) and ranks after this close.
                    let cursor = crate::EffectGroupHandle::restored(
                        handle.group_key(),
                        handle.children(),
                        handle.consumed(),
                    )?;
                    if let Err(error) = controller
                        .close_effect_group(handle, LoserPolicy::Cancel)
                        .await
                    {
                        tracing::warn!(
                            error = %error,
                            "closing a cancelled tool-child group failed; the close is retryable"
                        );
                    }
                    // The close has decided every undecided child; only now
                    // does the recorded cancellation reach the children's
                    // cooperative stop, as the group's cancel always did.
                    if lost_to_turn_gate {
                        self.note_turn_cancelled();
                    }
                    // The group stays the opener's: a rank that lands after
                    // this close — a committed loser's drain, a cancelled
                    // attempt's captured usage — is a fact the opener's end
                    // incorporates, and the end finalizes the group before the
                    // opener's outcome and accounting commit (§6, §7, §13).
                    // The cursor starts after the prefix just incorporated.
                    self.retain_outstanding_group(cursor);
                    // A child that parked this run's execution slot
                    // (`release_process_execution_permit_while`) is no longer
                    // awaited: the run continues, so it re-takes its slot here
                    // exactly as the batch-effect path does after a cancel
                    // grace drops a child future mid-wait.
                    crate::runtime::ensure_process_execution_permit().await;
                    return Ok(ToolChildGroupSettled {
                        settled,
                        settlement_positions,
                        decided: None,
                        cancelled: true,
                    });
                }
                Err(error) => {
                    self.retain_outstanding_group(handle);
                    return Err(error);
                }
            };
            let position = settlement.position;
            let child = match (children.get(position), settlement.outcome) {
                (
                    Some(PreparedGroupChild::Tool(leaf)),
                    Ok(crate::RuntimeEffectOutcome::ToolInvocation {
                        outcome,
                        settlement,
                    }),
                ) => match self
                    .apply_tool_child_settlement(leaf, *outcome, *settlement)
                    .await
                {
                    Ok(completed) => GroupChildSettled::Tool(Box::new(completed)),
                    Err(error) => {
                        self.retain_outstanding_group(handle);
                        return Err(error);
                    }
                },
                (
                    Some(PreparedGroupChild::Timer { .. }),
                    Ok(crate::RuntimeEffectOutcome::Sleep),
                ) => GroupChildSettled::Timer,
                (_, Ok(other)) => {
                    let error = crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                        format!(
                            "durable effect group {} settled position {position} with a {} \
                             outcome, which is not what that child was admitted as",
                            handle.group_key(),
                            other.kind().as_str(),
                        ),
                    );
                    self.retain_outstanding_group(handle);
                    return Err(error);
                }
                // A child that refused with a live fault (`ControllerAborted`:
                // its attempt's journal claim, renew or finalize failed) is a
                // refusal, not the tool's settlement, even where the group
                // sealed it as the child's `Failed` terminal. It aborts the
                // turn as the live fault it is (FIG-3528, FIG-3575); only a
                // child's recorded outcome stays on the result surface.
                (_, Err(mut error)) => {
                    if error.code.turn_failure_cause().aborts_invocation() {
                        error.journaled = false;
                    }
                    self.retain_outstanding_group(handle);
                    return Err(error);
                }
            };
            let decides = decided.is_none() && consumer.decides(child.fulfilled());
            settled[position] = Some(child);
            settlement_positions.push(position);
            if decides {
                decided = Some(position);
            }
            if decides && !handle.is_exhausted() {
                // Journal and apply the consumed prefix, then hand the group
                // to the opener: the losers run on while the opener lives, and
                // the opener's end closes them (ADR 0099 §0, §6, §7).
                if let Err(error) = self.incorporate_group_prefix(&handle).await {
                    self.retain_outstanding_group(handle);
                    return Err(error);
                }
                self.retain_outstanding_group(handle);
                return Ok(ToolChildGroupSettled {
                    settled,
                    settlement_positions,
                    decided: Some(position),
                    cancelled: false,
                });
            }
        }
        // Journal and apply the consumed prefix: every settled rank's facts
        // land once under its recorded `child_replay_key`, and a replay
        // re-incorporates exactly this prefix (FIG-3411 part 2).
        self.incorporate_group_prefix(&handle).await?;
        // Every child ranked, was consumed and is incorporated: the opener no
        // longer depends on the group (ADR 0099 §9's release condition).
        self.release_group_work(handle.group_key());
        if let Err(error) = controller
            .close_effect_group(handle, LoserPolicy::RunToCompletion)
            .await
        {
            tracing::warn!(
                error = %error,
                "closing a consumed tool-child group failed; the disposition is durable and the close is retryable"
            );
        }
        Ok(ToolChildGroupSettled {
            settled,
            settlement_positions,
            decided,
            cancelled: false,
        })
    }

    /// FIG-3411 seam: returns one settled child's completed call and emits its
    /// activity events.
    ///
    /// The once-only channels — possession, committed checkpoint messages,
    /// trigger receipts, and usage deltas charged once per
    /// `UsageDeltaIdentity` (ADR 0099 §6/§13) — are incorporated by the
    /// consumer through `incorporate_group_prefix`, which journals the
    /// `IncorporateGroupSettlements` record covering the consumed prefix
    /// (FIG-3411 part 2). Presentation is taken verbatim from
    /// `settlement.model_return` — the child's own recorded projection, run
    /// once inside the child — rather than re-running the projector. Realized
    /// intent outcomes and their activity events ride the carried
    /// `ToolDispatchOutcome`.
    ///
    /// Deliberately absent here, and owned by FIG-3411's remaining steps:
    /// cross-invocation carriage of the settlement facts (ADR 0099 §8).
    pub(crate) async fn apply_tool_child_settlement(
        &self,
        leaf: &PreparedToolChildLeaf,
        outcome: ToolDispatchOutcome,
        settlement: crate::runtime::ToolSettlement,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        let call_id = leaf.call.call.call_id.clone();
        let correlation_id = tool_activity_id(&call_id);
        // Settlement *facts* (possession, messages, triggers, usage) are not
        // applied here: `consume_tool_child_group` incorporates the
        // consumed prefix through `incorporate_group_prefix` (FIG-3411 part
        // 2), which journals the `IncorporateGroupSettlements` record and
        // names each rank's real `child_replay_key` — a replay re-incorporates
        // exactly the recorded prefix, never a rank that settled after the
        // record was cut. This seam keeps presentation (the recorded
        // `settlement.model_return`) and the activity events.
        // A child that ran with no live opener recorded its stream instead of
        // sending it (FIG-3712); it reaches the stream here, before the
        // child's own completion.
        self.emit_recorded_child_stream(&call_id, &outcome.record, &settlement.stream);
        {
            let mut cursor = self.observation_cursor(&format!("tool:{call_id}:intents"));
            for intent_outcome in &outcome.intent_outcomes {
                cursor.observe(
                    self.dispatch.observer.as_ref(),
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(correlation_id.clone()),
                        event: TurnEvent::ToolIntentOutcome {
                            call_id: call_id.clone(),
                            outcome: intent_outcome.clone(),
                        },
                    },
                );
            }
        }
        let record = ToolCallRecord {
            call_id: Some(call_id.clone()),
            tool: outcome.record.tool.clone(),
            args: outcome.record.args.clone(),
            output: outcome.record.output.clone(),
            duration_ms: outcome.record.duration_ms,
        };
        self.emit_tool_call_completed(&record, &outcome.attempts);
        Ok(CompletedProtocolToolCall {
            completed: crate::sansio::CompletedToolCall {
                call_id,
                tool_name: outcome.record.tool,
                args: outcome.record.args,
                output: outcome.record.output,
                model_return: settlement.model_return,
                duration_ms: outcome.record.duration_ms,
                intent_outcomes: outcome.intent_outcomes,
                replay: leaf.call.call.replay.clone(),
            },
            record,
        })
    }

    /// The standard-protocol driver's group entry point: opens one group over
    /// prepared calls that carry catalog admission only (a turn's prepared
    /// calls hold no grant), consumes it to exhaustion, and returns each leaf's
    /// completed call keyed by its input index.
    ///
    /// `group_invocation` is minted by the caller — the turn driver passes
    /// `causal::turn_tool_group_invocation`, whose replay key names the
    /// physical turn and protocol iteration (ADR 0099 §3).
    /// Formation and infrastructure failures surface as the controller error
    /// the caller maps onto its existing error type; a cancelled turn yields
    /// cancelled completions, not an error.
    pub async fn execute_prepared_tool_group(
        &self,
        batch_id: &str,
        group_invocation: crate::RuntimeEffectInvocation,
        prepared_entries: Vec<(usize, crate::PreparedToolCall)>,
    ) -> Result<Vec<(usize, CompletedProtocolToolCall)>, crate::RuntimeEffectControllerError> {
        let batch = crate::PreparedToolBatch::new(
            batch_id,
            prepared_entries
                .iter()
                .map(|(_, prepared)| prepared.clone())
                .collect(),
        );
        let mut leaves = Vec::with_capacity(prepared_entries.len());
        for ((input_index, _), call) in prepared_entries.into_iter().zip(batch.calls) {
            let manifest = crate::tool_dispatch::resolve_callable_manifest_by_id(
                self.dispatch.as_ref(),
                &call.call.tool_id,
            )
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!(
                        "tool id `{}` resolved no callable manifest between preparation and \
                         group formation; the batch cannot admit a child it cannot name",
                        call.call.tool_id
                    ),
                )
            })?;
            leaves.push(PreparedGroupChild::Tool(Box::new(PreparedToolChildLeaf {
                input_index,
                call,
                admission: ToolChildAdmission::Catalog {
                    manifest: Box::new(manifest),
                },
            })));
        }
        let consumer = ToolAggregateConsumer::AllSettled;
        let handle = self
            .open_tool_child_group(
                group_invocation,
                self.tool_child_group_key(batch_id),
                batch_id,
                &leaves,
                consumer.wake(),
                crate::GroupReopen::RetainedShape,
            )
            .await?;
        let mut settled = self
            .consume_tool_child_group(handle, &leaves, consumer)
            .await?;
        let mut results = Vec::with_capacity(leaves.len());
        for (position, leaf) in leaves.iter().enumerate() {
            let (Some(leaf), Some(GroupChildSettled::Tool(completed))) =
                (leaf.tool(), settled.settled[position].take())
            else {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!(
                        "tool-child group {batch_id} consumed position {position} without \
                         filling it with a tool call"
                    ),
                ));
            };
            results.push((leaf.input_index, *completed));
        }
        Ok(results)
    }
}

/// The completed call a cancelled await fills unsettled positions with: the
/// batch surface's cancelled reply (batch.rs's `"tool call cancelled"` shape)
/// as a `CompletedProtocolToolCall`.
fn cancelled_group_leaf(leaf: &PreparedToolChildLeaf) -> CompletedProtocolToolCall {
    let completed = cancelled_completed_tool_call(
        leaf.call.call.call_id.clone(),
        leaf.call.call.tool_name.clone(),
        leaf.call.call.args.clone(),
        leaf.call.call.replay.clone(),
    );
    let record = ToolCallRecord {
        call_id: Some(completed.call_id.clone()),
        tool: completed.tool_name.clone(),
        args: completed.args.clone(),
        output: completed.output.clone(),
        duration_ms: completed.duration_ms,
    };
    CompletedProtocolToolCall { completed, record }
}

/// What a group tool child records of its opener and emits back to it
/// (FIG-3712).
impl RuntimeExecutionContext<'_> {
    /// Emits the stream events a group child recorded because no opener was
    /// live where it ran (FIG-3712): its session events on the session
    /// stream, its turn activities on the activity stream, each in recorded
    /// order and each verbatim — the events carry the identities the child's
    /// recorder minted, so they publish through `Recorded*` variants rather
    /// than being re-projected or re-keyed. A stream the recording budget
    /// cut says so on the session stream, as a `child_stream_truncated`
    /// message.
    fn emit_recorded_child_stream(
        &self,
        call_id: &str,
        record: &crate::ToolCallRecord,
        stream: &crate::runtime::effect::RecordedChildStream,
    ) {
        let record = serde_json::to_value(record).unwrap_or_default();
        let (events, undecodable) = stream.decode(&record);
        if undecodable > 0 {
            tracing::warn!(
                call_id,
                undecodable,
                "a tool child's recorded stream held events this build cannot decode; \
                 they are skipped"
            );
        }
        let mut cursor = self.observation_cursor(&format!("tool:{call_id}:stream"));
        let observer = self.dispatch.observer.as_ref();
        for event in events {
            match event {
                crate::runtime::effect::DecodedChildEvent::Session(event) => {
                    cursor.observe(
                        observer,
                        crate::engine::ObservedEvent::RecordedSession(event),
                    );
                }
                crate::runtime::effect::DecodedChildEvent::Activity(activity) => {
                    cursor.observe(
                        observer,
                        crate::engine::ObservedEvent::RecordedActivity(activity),
                    );
                }
            }
        }
        if let Some(truncated) = stream.truncated {
            cursor.observe(
                observer,
                crate::engine::ObservedEvent::RecordedSession(crate::SessionStreamEvent::Message {
                    text: format!(
                        "tool child `{call_id}` recorded more stream than its budget holds; \
                             {} later events ({} bytes) were dropped",
                        truncated.dropped_events, truncated.dropped_bytes
                    ),
                    kind: "child_stream_truncated".to_string(),
                }),
            );
        }
    }

    /// The session facts a group tool child this context opens records
    /// (FIG-3712): the tool surface its calls are admitted against, the
    /// session's tool access and subagent context, and which of this
    /// context's sources have no recorded form.
    pub(crate) fn tool_child_session_facts(&self) -> crate::runtime::effect::ToolChildSessionFacts {
        let plugins = &self.dispatch.plugins;
        crate::runtime::effect::ToolChildSessionFacts {
            tool_surface: self
                .dispatch
                .tool_catalog
                .tools
                .iter()
                .map(|entry| crate::ToolDefinition {
                    manifest: entry.manifest.clone(),
                    contract: entry.contract.as_ref().clone(),
                })
                .collect(),
            tool_access: plugins.tool_access(),
            subagent: plugins.subagent_context().cloned(),
            unrecorded: self.unrecorded_sources.union(
                crate::runtime::effect::UnrecordedSessionSources {
                    fork_plugins: plugins.forked_plugins(),
                    plugin_state: plugins.holds_plugin_state(),
                    ..Default::default()
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host-code batch's group key carries its batch id exactly once, and
    /// two different call lists mint two keys.
    #[test]
    fn the_group_key_carries_the_batch_id_once() {
        let calls = |id: &str| {
            vec![ToolInvocation::new(
                id,
                crate::ToolId::from("tool:one"),
                serde_json::json!({}),
            )]
        };
        let first_id = deterministic_tool_invocation_batch_id(&calls("call-1"));
        let second_id = deterministic_tool_invocation_batch_id(&calls("call-2"));
        assert_ne!(first_id, second_id);

        let context = crate::testing::TestExecutionContextBuilder::over_controller(
            std::sync::Arc::new(crate::testing::UnavailableEffectController)
                as std::sync::Arc<dyn crate::RuntimeEffectController>,
        )
        .build()
        .into_runtime();
        let first = context.tool_child_group_key(&first_id);
        let second = context.tool_child_group_key(&second_id);
        assert_ne!(first, second);
        assert_eq!(first, context.tool_child_group_key(&first_id));
        assert_eq!(
            first.matches(first_id.as_str()).count(),
            1,
            "the batch id appears in the group key exactly once"
        );
        assert_eq!(first.matches(second_id.as_str()).count(), 0);
    }
}
