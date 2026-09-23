//! Effect-group formation and settlement consumption for tool batches
//! (ADR 0099, FIG-3397).
//!
//! This module replaces the batch-effect consumer path: a prepared batch of
//! tool calls is opened as a [`crate::RuntimeEffectGroup`] whose children are
//! [`crate::RuntimeEffectCommand::ToolInvocation`] envelopes, and the opener
//! consumes the group's durable settlement order rather than a source-ordered
//! launch vector. What changes under ADR 0099 §5 is cross-child ordering: a
//! child's final record commits first, its drain admits in durable
//! `commit_seq` order, and the consumer observes settlement rank — so a held
//! source-first leaf no longer blocks a later sibling's terminal.
//!
//! A cancelled turn closes its group under `Cancel` through the FIG-3410
//! closing driver. What does *not* live here yet: opener-side lifecycle
//! closing at turn end and process terminal with the opener's own
//! finalization steps (FIG-3410, ADR 0099 §7) and drain-end handling
//! (FIG-3419).
//! Settlement-fact incorporation is FIG-3411's `incorporate_group_prefix`:
//! the consumer journals the consumed prefix as an `IncorporateGroupSettlements`
//! record and applies each rank's recorded facts once, while the seam below
//! reads the model return the child projected and recorded rather than
//! re-projecting it (ADR 0099 §6).

use super::*;

use crate::runtime::effect::{
    GroupWakePolicy, LoserPolicy, ToolChildAdmission, ToolChildCompletionRouting, ToolChildRequest,
    ToolChildScope,
};

/// One prepared leaf of a tool-child group: everything formation needs to
/// mint the child's retained [`ToolChildRequest`] and the consumer needs to
/// apply its settlement.
///
/// `input_index` is the leaf's position in the caller's original call vector;
/// the position inside the group's `children` is the leaf's index in the
/// vector passed to [`RuntimeExecutionContext::open_tool_child_group`]. The
/// position-to-input mapping is the only translation PR C's dedup needs to
/// change (ADR 0099 §10: duplicate operands ride one position-to-unique map).
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

/// What [`RuntimeExecutionContext::consume_all_tool_child_settlements`] hands
/// back: every leaf's completed call, plus the settlement order the group
/// actually produced.
///
/// `settled` is indexed by group position; `settlement_positions` is the
/// rank-ordered list of positions the group settled, which the batch surface
/// maps back to input indices for `ToolBatchReplies::settlement_order`.
pub(crate) struct ToolChildGroupSettled {
    /// One slot per group position; every slot is filled — unconsumed
    /// positions carry the cancelled completion a turn-cancel await produced.
    pub settled: Vec<Option<CompletedProtocolToolCall>>,
    /// Positions in the order the group settled them (durable commit order,
    /// ADR 0099 §5), with cancel-unconsumed positions appended in position
    /// order so `validate_batch_settlement_order` still sees a permutation.
    pub settlement_positions: Vec<usize>,
}

impl RuntimeExecutionContext<'_> {
    /// The group key one batch's children are journaled under:
    /// `{scope_id}:group:{batch_id}` for a top-level batch and
    /// `{scope_id}:group:{parent_effect_id}:{batch_id}` for a nested one,
    /// mirroring how [`Self::tool_batch_invocation`] prefixes a nested batch.
    ///
    /// The occurrence ordinal is carried exactly once: `batch_id` is minted by
    /// `tool_invocation_batch_preimage` under `TOOL_BATCH_FAMILY_VERSION` 2,
    /// which already folds `ToolGroupOccurrence` into the hash (FIG-3394), so
    /// the key has no separate occurrence segment.
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
    /// that refuses a retired owner, a `ProcessLifetime` routing with no known
    /// issuer, and a `DurableJournaled` participant whose controller names no
    /// await-event authority all return
    /// [`crate::RuntimeErrorCode::RuntimeEffectGroupShape`].
    ///
    /// Completion routing mirrors what `coordinate_tool_invocation` re-derives
    /// per attempt (attempt_coordinator.rs): `may_defer` is asked of the same
    /// `attempt_may_defer` seam, and the answer is *recorded* — a `NotNeeded`
    /// or `Unsupported` preparation records `Inline`, which forces the child's
    /// `may_defer` false so its attempt fails on a defer exactly as an
    /// `Unsupported` key does today; an `Issued` preparation records `Durable`
    /// for a durable-journaled controller and `ProcessLifetime { issuer }` for
    /// a local one, where `issuer` is the effect host's
    /// `turn_control_binding_id` threaded in through
    /// `with_tool_child_completion_issuer`. The attempt-side honour check
    /// refuses a recorded arm the controller would not have issued, so a
    /// replayed formation under a different deployment cannot silently
    /// reinterpret the routing.
    pub(crate) async fn open_tool_child_group(
        &self,
        group_invocation: crate::RuntimeEffectInvocation,
        batch_id: &str,
        leaves: &[PreparedToolChildLeaf],
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        let scoped = self.dispatch.effect_controller.scoped();
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

        let participation = controller.effect_journaling();
        let cancellation_authority = match participation {
            crate::EffectJournaling::Journaled => {
                let authority = controller
                    .await_event_authority_binding_id()
                    .ok_or_else(|| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                            "a durable-journaled tool-child group requires the controller to \
                         name its await-event authority; without it the child records no \
                         cancellation authority it can honour",
                        )
                    })?;
                let binding_id =
                    crate::runtime::turn_control_binding_id_for_scope(&authority, &scope)
                        .map_err(crate::RuntimeEffectControllerError::from)?;
                Some(
                    crate::TurnControlBindingId::new(binding_id).map_err(|error| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                            format!("the derived cancellation authority is not a binding: {error}"),
                        )
                    })?,
                )
            }
            crate::EffectJournaling::Local => None,
        };

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
        for leaf in leaves {
            let call_id = leaf.call.call.call_id.clone();
            self.emit_tool_call_started(
                &call_id,
                &leaf.call.call.tool_name,
                leaf.call.call.args.clone(),
                tool_activity_id(&call_id),
            )
            .await;
        }

        let group_key = self.tool_child_group_key(batch_id);
        let mut children = Vec::with_capacity(leaves.len());
        for (position, leaf) in leaves.iter().enumerate() {
            let call_id = leaf.call.call.call_id.clone();
            let completion_routing = self
                .tool_child_completion_routing(
                    controller,
                    &scope,
                    participation,
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
                execution_env.clone(),
                completion_routing,
            );
            if let Some(process_ref) = opener.process_ref() {
                request = request.with_enclosing_process(process_ref.clone());
            }
            if let Some(authority) = &cancellation_authority {
                request = request.with_cancellation_authority(authority.clone());
            }
            children.push(crate::RuntimeEffectEnvelope::new(
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
        let group = crate::RuntimeEffectGroup::try_new(
            group_invocation,
            group_key,
            children,
            GroupWakePolicy::All,
            LoserPolicy::RunToCompletion,
        )?;
        controller.open_effect_group(group).await
    }

    /// Records one leaf's completion routing from the same two admission facts
    /// the attempt coordinator consults: whether the tool may defer, and what
    /// the controller can issue for a completion key.
    async fn tool_child_completion_routing(
        &self,
        controller: &dyn crate::RuntimeEffectController,
        scope: &crate::ExecutionScope,
        participation: crate::EffectJournaling,
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
            crate::CompletionKeyPreparation::Issued(_) => match participation {
                crate::EffectJournaling::Journaled => Ok(ToolChildCompletionRouting::Durable),
                crate::EffectJournaling::Local => {
                    let issuer = self.tool_child_completion_issuer.clone().ok_or_else(|| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                            format!(
                                "tool child `{call_id}` was issued a process-lifetime \
                                 completion key, but this context knows no issuing registry; \
                                 a ProcessLifetime route needs the host's turn-control \
                                 binding id and is refused rather than downgraded to Inline"
                            ),
                        )
                    })?;
                    Ok(ToolChildCompletionRouting::ProcessLifetime { issuer })
                }
            },
        }
    }

    /// Consumes a tool-child group's settlement order to exhaustion
    /// (ADR 0099 §5, §10).
    ///
    /// Settlements are served by durable rank, so `settled[position]` fills in
    /// commit order — not input order. A `GroupSettlement.outcome: Err` is
    /// infrastructure, never a tool rejection (§10: tool rejections arrive as
    /// ordinary `ToolCallOutput`s inside the outcome), and aborts the consume
    /// with the controller error so the batch surface fails closed.
    ///
    /// A `RuntimeEffectGroupAwaitCancelled` await means the turn was
    /// cancelled: consumption stops, every unsettled position is filled with
    /// the batch surface's cancelled reply and appended to the settlement
    /// order in position order, and the group is closed under
    /// [`LoserPolicy::Cancel`]: the close records `closing` through the
    /// FIG-3410 driver, seats a cancelled terminal for every undecided child
    /// and fires the group's token, so a child that ignores cooperative
    /// cancellation is dropped as the batch path's cancel grace dropped it.
    /// Wiring the opener's real `OpenerFinalizationSteps` — whose
    /// `commit_outcome_and_accounting` is FIG-3411 step 2 — and closing at
    /// turn end / process terminal remain PR C's.
    ///
    /// On clean exhaustion the group is closed under the declared
    /// `RunToCompletion` disposition to release consumer interest: the close
    /// CASes the journaled `Live → Closing` fact and returns without waiting —
    /// finalization is host-owned and cursor-resumable — and it runs against
    /// `GroupOnlyFinalization`, so a consumer-only close owes no opener steps.
    /// A close error is logged rather than failing the batch, because close is
    /// idempotent and retryable by the trait contract and the settlements are
    /// already consumed.
    pub(crate) async fn consume_all_tool_child_settlements(
        &self,
        mut handle: crate::EffectGroupHandle,
        leaves: &[PreparedToolChildLeaf],
    ) -> Result<ToolChildGroupSettled, crate::RuntimeEffectControllerError> {
        let controller = self.dispatch.effect_controller.controller();
        let cancel = self.cancellation_token.clone().unwrap_or_default();
        let mut settled: Vec<Option<CompletedProtocolToolCall>> =
            (0..leaves.len()).map(|_| None).collect();
        let mut settlement_positions = Vec::with_capacity(leaves.len());
        while !handle.is_exhausted() {
            let settlement = match controller
                .await_next_settlement(&mut handle, cancel.child_token())
                .await
            {
                Ok(settlement) => settlement,
                Err(error)
                    if error.code == crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled =>
                {
                    for (position, leaf) in leaves.iter().enumerate() {
                        if settled[position].is_none() {
                            settled[position] = Some(cancelled_group_leaf(leaf));
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
                    // finish its drain (§4); finalization is host-owned.
                    if let Err(error) = controller
                        .close_effect_group(handle, LoserPolicy::Cancel)
                        .await
                    {
                        tracing::warn!(
                            error = %error,
                            "closing a cancelled tool-child group failed; the close is retryable"
                        );
                    }
                    // A child that parked this run's execution slot
                    // (`release_process_execution_permit_while`) is no longer
                    // awaited: the run continues, so it re-takes its slot here
                    // exactly as the batch-effect path does after a cancel
                    // grace drops a child future mid-wait.
                    crate::runtime::ensure_process_execution_permit().await;
                    return Ok(ToolChildGroupSettled {
                        settled,
                        settlement_positions,
                    });
                }
                Err(error) => return Err(error),
            };
            let position = settlement.position;
            match settlement.outcome {
                Ok(crate::RuntimeEffectOutcome::ToolInvocation {
                    outcome,
                    settlement,
                }) => {
                    let completed = self
                        .apply_tool_child_settlement(&leaves[position], *outcome, *settlement)
                        .await?;
                    settled[position] = Some(completed);
                }
                Ok(other) => {
                    return Err(crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                        format!(
                            "durable effect group {} settled position {position} with a {} \
                             outcome; a tool-child group settles only ToolInvocation children",
                            handle.group_key(),
                            other.kind().as_str(),
                        ),
                    ));
                }
                Err(error) => return Err(error),
            }
            settlement_positions.push(position);
        }
        // Journal and apply the consumed prefix: every settled rank's facts
        // land once under its recorded `child_replay_key`, and a replay
        // re-incorporates exactly this prefix (FIG-3411 part 2).
        self.incorporate_group_prefix(&handle).await?;
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
        // applied here: `consume_all_tool_child_settlements` incorporates the
        // consumed prefix through `incorporate_group_prefix` (FIG-3411 part
        // 2), which journals the `IncorporateGroupSettlements` record and
        // names each rank's real `child_replay_key` — a replay re-incorporates
        // exactly the recorded prefix, never a rank that settled after the
        // record was cut. This seam keeps presentation (the recorded
        // `settlement.model_return`) and the activity events.
        for intent_outcome in &outcome.intent_outcomes {
            self.emit_turn_activity(
                correlation_id.clone(),
                TurnEvent::ToolIntentOutcome {
                    call_id: call_id.clone(),
                    outcome: intent_outcome.clone(),
                },
            )
            .await;
        }
        let record = ToolCallRecord {
            call_id: Some(call_id.clone()),
            tool: outcome.record.tool.clone(),
            args: outcome.record.args.clone(),
            output: outcome.record.output.clone(),
            duration_ms: outcome.record.duration_ms,
        };
        self.emit_tool_call_completed(&record, &outcome.attempts)
            .await;
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
            leaves.push(PreparedToolChildLeaf {
                input_index,
                call,
                admission: ToolChildAdmission::Catalog {
                    manifest: Box::new(manifest),
                },
            });
        }
        let handle = self
            .open_tool_child_group(group_invocation, batch_id, &leaves)
            .await?;
        let mut settled = self
            .consume_all_tool_child_settlements(handle, &leaves)
            .await?;
        let mut results = Vec::with_capacity(leaves.len());
        for (position, leaf) in leaves.iter().enumerate() {
            let completed = settled.settled[position].take().ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!(
                        "tool-child group {} consumed position {position} without filling it",
                        batch_id
                    ),
                )
            })?;
            results.push((leaf.input_index, completed));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The occurrence ordinal rides inside `batch_id` (FIG-3394), so the group
    /// key needs no occurrence segment of its own: two batches that differ
    /// only in occurrence mint different batch ids, one batch mints one key,
    /// and the key contains that id exactly once.
    #[test]
    fn the_group_key_carries_the_occurrence_once_through_the_batch_id() {
        let calls = || {
            vec![ToolInvocation::new(
                "call-1",
                crate::ToolId::from("tool:one"),
                serde_json::json!({}),
            )]
        };
        let first_id = deterministic_tool_invocation_batch_id(
            &calls(),
            crate::session::ToolGroupOccurrence::Opener(1),
        );
        let second_id = deterministic_tool_invocation_batch_id(
            &calls(),
            crate::session::ToolGroupOccurrence::Opener(2),
        );
        assert_ne!(
            first_id, second_id,
            "identical call lists under different occurrences mint different batch ids"
        );

        let context = crate::testing::code_execution_context();
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
