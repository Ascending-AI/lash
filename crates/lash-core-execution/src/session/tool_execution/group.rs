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
//! closing driver; a batch surface then presents every member it had not
//! consumed by that member's durable final, never by the cancelled cursor.
//!
//! Settlement facts are incorporated through FIG-3411's
//! `incorporate_group_prefix`: the consumer journals the consumed prefix as
//! an `IncorporateGroupSettlements` record and applies each rank's recorded
//! facts once, while presentation reads the model return the child projected
//! and recorded rather than re-projecting it (§6).

/// version_surface = "coexist"
/// version_guard(items(retained_request_digest))
const TOOL_CALL_REQUEST_FAMILY_VERSION: u8 = 1;

use super::*;
use lash_sansio::sync::MutexExt;

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
    /// The prepared call, produced by
    /// [`crate::PreparedToolBatch::new_with_grants`]. Its `ToolCallId` keys
    /// every attempt the child makes (ADR 0117 §6).
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
}

impl PreparedGroupChild {
    /// The tool leaf, when this child is one.
    pub(crate) fn tool(&self) -> Option<&PreparedToolChildLeaf> {
        match self {
            Self::Tool(leaf) => Some(leaf),
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
    Deferred(Box<crate::tool_dispatch::DeferredToolCompletion>),
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
            Self::Deferred(_) => false,
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
    /// after a decision or a cancelled await, only the consumed prefix is
    /// filled — a value is never synthesized (§10 L6). A surface that
    /// presents a cancelled group's rows fills the rest from the durable
    /// finals with [`RuntimeExecutionContext::present_cancelled_tool_group`].
    pub settled: Vec<Option<GroupChildSettled>>,
    /// Positions in the order the group settled them: durable rank order
    /// (ADR 0099 §5), one per filled slot.
    pub settlement_positions: Vec<usize>,
    /// The await was cancelled with the turn.
    pub cancelled: bool,
}

impl RuntimeExecutionContext<'_> {
    /// The group key one batch's children are journaled under:
    /// `{scope_id}:group:{batch_id}` for a top-level batch and
    /// `{scope_id}:group:{parent_effect_id}:{batch_id}` for one opened under a
    /// parent effect, mirroring how [`Self::tool_batch_invocation`] prefixes
    /// it.
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
    /// journal needs to reconstruct and redrive the child with no opener in
    /// scope: the pinned admission, the batch-derived attempt identity (so
    /// attempt envelopes hash identically to the pre-group batch path), the
    /// checked opener/scope pair, the recorded cancellation authority, the
    /// execution-environment reference published under
    /// the scope's execution referrer, and the completion routing the admission
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
    /// per attempt (attempt_coordinator.rs): `may_defer` is the admitted
    /// manifest's declaration, and the answer is *recorded* — a `NotNeeded`
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
        let scoped = self.dispatch.effect_controller.clone();
        // Opening a group writes the journal; a replayed language command's
        // guard admits it or refuses it before any row exists (FIG-3586).
        scoped.admit_journal_write()?;
        // The group's unique tool calls are admitted against the session's
        // recorded `max_tool_calls` before anything of it is journaled,
        // announced or dispatched (ADR 0099 §9, FIG-4546). Timers are not
        // tool calls and are not counted.
        let tool_calls = children
            .iter()
            .filter(|child| child.tool().is_some())
            .count();
        self.reserve_tool_calls(&group_key, tool_calls).await?;
        self.form_tool_child_group(
            group_invocation,
            group_key.clone(),
            batch_id,
            children,
            wake,
            reopen,
        )
        .await
        .inspect_err(|_| self.release_group_work(&group_key))
    }

    /// Forms and opens a group whose tool calls are already admitted.
    async fn form_tool_child_group(
        &self,
        group_invocation: crate::RuntimeEffectInvocation,
        group_key: String,
        batch_id: &str,
        children: &[PreparedGroupChild],
        wake: GroupWakePolicy,
        reopen: crate::GroupReopen,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        let scoped = self.dispatch.effect_controller.clone();
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

        // What this opener lends is this process's to judge, and the children
        // record it (FIG-4590). A context with no tool-child host has no
        // resolver that reads the answer, and records the default.
        let opener_context = self
            .tool_children
            .as_ref()
            .map(|tool_children| {
                tool_children.pin_open_tool_group(
                    &group_key,
                    &opener,
                    children
                        .iter()
                        .enumerate()
                        .filter_map(|(position, child)| child.tool().map(|_| position)),
                )
            })
            .unwrap_or_default();

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
            .captured_process_execution_env_ref(
                &crate::session::execution_context::execution_claim_of(&scope).map_err(
                    |error| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                            format!("tool-child group formation has no execution referrer: {error}"),
                        )
                    },
                )?,
            )
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!("tool-child group formation could not publish its execution environment: {error}"),
                )
            })?;

        // Each child's request is bound to its identity before anything of
        // the group is journaled (ADR 0117 §7).
        let requests = children
            .iter()
            .map(|child| child.tool().map(|leaf| (&leaf.call.call, &leaf.admission)))
            .collect::<Vec<_>>();
        let retained_payloads = self.bind_retained_requests(&group_key, &requests).await?;

        // Live trace/activity is the opener's (ToolSettlement plan rule 3):
        // started events are emitted here, at formation, exactly as the batch
        // path emitted them at each child's dispatch. Each lane keys on the
        // child's own invocation replay key — `{group}:child:{position}`,
        // minted below (ADR 0105 §1).
        for (position, leaf) in children.iter().enumerate() {
            let Some(leaf) = leaf.tool() else {
                continue;
            };
            let ids = crate::tool_dispatch::ToolCallIds::of(&leaf.call.call);
            self.emit_tool_call_started(
                &group_child_replay_key(&group_key, position),
                &ids,
                &leaf.call.call.tool_name,
                leaf.call.call.args.clone(),
                tool_activity_id(&ids.call_id),
            )
            .await;
        }

        let session_facts = self.tool_child_session_facts(opener_context);
        let mut envelopes = Vec::with_capacity(children.len());
        for (position, child) in children.iter().enumerate() {
            let leaf = match child {
                PreparedGroupChild::Tool(leaf) => leaf,
            };
            let call_id = leaf.call.call.call_id.clone();
            let mut call = leaf.call.call.clone();
            if let Some(Some(retained)) = retained_payloads.get(position) {
                call.prepared_payload = retained.clone();
            }
            let completion_routing = self
                .tool_child_completion_routing(
                    controller,
                    &scope,
                    leaf.admission.manifest().declaration.may_defer,
                    &call_id,
                )
                .await?;
            let request = ToolChildRequest::new(
                call,
                leaf.admission.clone(),
                crate::tool_dispatch::ToolAttemptLineage::under(
                    group_invocation.clone().into_runtime_invocation(),
                ),
                ToolChildScope {
                    opener: opener.clone(),
                    owner: self.dispatch.owner.clone(),
                },
                cancellation_authority.clone(),
                execution_env.clone(),
                completion_routing,
                session_facts.clone(),
            )
            .with_trace_request(self.tool_requests.lock_recover().get(&call_id).cloned());
            envelopes.push(crate::RuntimeEffectEnvelope::new(
                crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(
                        scope.clone(),
                        group_child_replay_key(&group_key, position),
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
        .map(|group| group.with_reopen(reopen))?;
        controller.open_effect_group(group).await
    }

    /// Binds every tool child's request to its call's identity (ADR 0117 §7)
    /// and returns, by position, the prepared payload each call runs with.
    ///
    /// The first formation journals, in one record under `{group}:requests`,
    /// each tool child's call id and the digest of its canonical tool,
    /// arguments and authority, together with the payload its prepare phase
    /// sealed. One record, not one per child, so a replay of the opener
    /// resumes a bounded number of times whatever the group's width. Every
    /// later formation — a replay of the opener, a redrive — runs each call
    /// from its retained payload, whatever a fresh prepare sealed, so no
    /// effect runs with a payload other than the one sealed at admission. A
    /// request whose tool, arguments or authority differ under the same id is
    /// refused before any effect, through the binding-drift refusal a drifted
    /// tool meets (ADR 0116 §2.2), so the turn parks; the id is never reminted
    /// to fit.
    pub(crate) async fn bind_retained_requests(
        &self,
        group_key: &str,
        requests: &[Option<(&crate::PreparedToolCall, &ToolChildAdmission)>],
    ) -> Result<Vec<Option<serde_json::Value>>, crate::RuntimeEffectControllerError> {
        let formed = requests
            .iter()
            .map(|request| {
                request.map(|(call, admission)| (call, retained_request_digest(call, admission)))
            })
            .collect::<Vec<_>>();
        if formed.iter().all(Option::is_none) {
            return Ok(vec![None; requests.len()]);
        }
        let Some(store) = self.dispatch.tool_receipts.clone() else {
            return Ok(formed
                .iter()
                .map(|formed| {
                    formed
                        .as_ref()
                        .map(|(call, _)| call.prepared_payload.clone())
                })
                .collect());
        };
        let owner = crate::EffectOpener::for_scope(&self.admitted_scope()).map_err(|error| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                error.to_string(),
            )
        })?;
        let owner = crate::trace::run_receipts::tool_owner(&owner);
        let offers = formed
            .iter()
            .map(|formed| {
                formed
                    .as_ref()
                    .map(|(call, digest)| ((*call).clone(), digest.clone()))
            })
            .collect::<Vec<_>>();
        let tracing = self.tracing.clone();
        let standing = tracing
            .as_ref()
            .map(|tracing| self.coordination_standing(tracing));
        let clock = self.dispatch.clock.clone();
        let context = tracing
            .as_ref()
            .map(|tracing| tracing.scope_context.clone())
            .unwrap_or_default();
        let parent = tracing.as_ref().and_then(|tracing| tracing.scope.clone());
        let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
        let execution_scope_id = self.execution_scope_id().to_string();
        let recorded = self
            .journaled_language_value_with(
                format!(
                    "{group_key}:{}",
                    crate::runtime::causal::CommandSubKey::AggregateRequests
                ),
                "retain-tool-requests".to_string(),
                move || async move {
                    let mut records = Vec::with_capacity(offers.len());
                    for offered in offers {
                        let Some((call, digest)) = offered else {
                            records.push(serde_json::Value::Null);
                            continue;
                        };
                        let at_ms = clock.timestamp_ms();
                        let mut candidate = None;
                        let scope = parent
                            .as_ref()
                            .and_then(|parent| {
                                crate::trace::tool_trace_scope(parent, &call.call_id, at_ms)
                            })
                            .map(|mut scope| {
                                if let Some(tracing) = &tracing {
                                    let proposed = tracing
                                        .runtime
                                        .scopes()
                                        .propose(&scope.scope, &scope.cause);
                                    scope.anchor = proposed.anchor();
                                    candidate = Some(proposed);
                                }
                                scope
                            });
                        let request = crate::store::ToolRequestReceipt {
                            owner: owner.clone(),
                            request_key: format!("{}:{}", execution_scope_id, call.call_id),
                            payload_digest: digest,
                            payload: serde_json::to_value(&call).map_err(|e| {
                                crate::RuntimeEffectControllerError::new(
                                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                                    e.to_string(),
                                )
                            })?,
                            scope,
                            context: context.clone(),
                            requested_at_ms: at_ms,
                        };
                        let receipt = store.record_tool_request(&request).await;
                        if let Some(candidate) = candidate {
                            candidate.settle(match &receipt {
                                Ok(receipt) if receipt.changed => {
                                    lash_trace::TraceCandidateOutcome::Selected
                                }
                                Ok(_) => lash_trace::TraceCandidateOutcome::Reused,
                                Err(_) => lash_trace::TraceCandidateOutcome::Refused,
                            });
                        }
                        let receipt = receipt.map_err(|error| match error {
                            crate::store::StoreError::ToolRequestConflict { .. } => {
                                retained_request_drift(&call)
                            }
                            other => crate::RuntimeEffectControllerError::from(other),
                        })?;
                        if let (Some(standing), Some(scope)) = (&standing, &receipt.record.scope) {
                            standing.under(scope.clone()).transition(
                                receipt.permit().as_ref(),
                                receipt.record.requested_at_ms,
                                lash_trace::TraceTransitionKind::Started,
                                0,
                                || {
                                    (
                                        receipt.record.context.clone(),
                                        lash_trace::TraceEvent::ToolCallStarted {
                                            call_id: call.call_id.clone(),
                                            provider_call_id: call.provider_call_id.clone(),
                                            name: call.tool_name.clone(),
                                            args: call.args.clone(),
                                            issuing_node_id: issuing_node.clone(),
                                        },
                                    )
                                },
                            );
                        }
                        records.push(serde_json::to_value(receipt.record).map_err(|e| {
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                                e.to_string(),
                            )
                        })?);
                    }
                    Ok(serde_json::Value::Array(records))
                },
            )
            .await?;
        let recorded = recorded.as_array().map(Vec::as_slice).unwrap_or_default();
        formed
            .iter()
            .enumerate()
            .map(|(position, formed)| {
                let Some((call, digest)) = formed else {
                    return Ok(None);
                };
                let receipt: crate::store::ToolRequestReceipt =
                    serde_json::from_value(recorded.get(position).cloned().unwrap_or_default())
                        .map_err(|_| retained_request_drift(call))?;
                if receipt.payload_digest != *digest {
                    return Err(retained_request_drift(call));
                }
                let retained: crate::PreparedToolCall =
                    serde_json::from_value(receipt.payload.clone())
                        .map_err(|_| retained_request_drift(call))?;
                if retained.call_id != call.call_id {
                    return Err(retained_request_drift(call));
                }
                self.tool_requests
                    .lock_recover()
                    .insert(call.call_id.clone(), receipt);
                Ok(Some(retained.prepared_payload))
            })
            .collect()
    }

    /// Records one leaf's completion routing from the same two admission facts
    /// the attempt coordinator consults: whether the admitted declaration may
    /// defer, and what the controller can issue for a completion key.
    async fn tool_child_completion_routing(
        &self,
        controller: &dyn crate::RuntimeEffectController,
        scope: &crate::ExecutionScope,
        may_defer: bool,
        call_id: &crate::ToolCallId,
    ) -> Result<ToolChildCompletionRouting, crate::RuntimeEffectControllerError> {
        if !may_defer {
            return Ok(ToolChildCompletionRouting::Inline);
        }
        match controller
            .prepare_completion_key(
                scope,
                crate::AwaitEventWaitIdentity::tool_completion(call_id.clone()),
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
    /// cancelled. Consumption stops with only the consumed prefix filled, and
    /// the group is closed under [`LoserPolicy::Cancel`]: the close records
    /// `closing` through the FIG-3410 driver, cancel-decides and seats every
    /// undecided child and fires the group's token without joining cancelled
    /// attempt bodies. A committed child keeps its authority to finish its drain
    /// (§4) and seats its own final. The group — with the cursor after its
    /// incorporated prefix — is then handed to the opener, whose end
    /// incorporates the ranks that land after the close. The consumer answers
    /// without waiting for those seats; a surface that presents one row per
    /// member reads them with [`Self::present_cancelled_tool_group`].
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
        handle: crate::EffectGroupHandle,
        children: &[PreparedGroupChild],
        consumer: ToolAggregateConsumer,
    ) -> Result<ToolChildGroupSettled, crate::RuntimeEffectControllerError> {
        self.consume_tool_child_group_mode(handle, children, consumer, false)
            .await
    }

    async fn consume_tool_child_group_mode(
        &self,
        mut handle: crate::EffectGroupHandle,
        children: &[PreparedGroupChild],
        consumer: ToolAggregateConsumer,
        dispatch_only: bool,
    ) -> Result<ToolChildGroupSettled, crate::RuntimeEffectControllerError> {
        let controller = self.dispatch.effect_controller.controller();
        let cancel = self.cancellation_token.clone().unwrap_or_default();
        // Each child's observed duration is the opener's wait window: the
        // child ran inside its journaled invocation and records no clock
        // facts, so the Completed activity carries this measured elapsed
        // (FIG-3696).
        let consume_started = self.dispatch.clock.now();
        let mut settled: Vec<Option<GroupChildSettled>> =
            (0..children.len()).map(|_| None).collect();
        let mut settlement_positions = Vec::with_capacity(children.len());
        let mut decided = None;
        let mut lost_to_turn_gate = false;
        let mut pending: Vec<(usize, Box<crate::tool_dispatch::DeferredToolCompletion>)> =
            Vec::new();
        while !handle.is_exhausted() || !pending.is_empty() {
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
                let completion = if pending.is_empty() {
                    None
                } else {
                    Some(
                        self.await_deferred_tool_completions(
                            &format!(
                                "{}:completion:{}:{}",
                                handle.group_key(),
                                handle.consumed(),
                                settlement_positions.len()
                            ),
                            pending
                                .iter()
                                .map(|(_, completion)| crate::ToolCompletionWait {
                                    key: completion.pending.key.clone(),
                                })
                                .collect(),
                            (!handle.is_exhausted()).then(|| crate::ToolDispatchCursor {
                                group_key: handle.group_key().to_owned(),
                                rank: handle.consumed() as u64 + 1,
                            }),
                            false,
                        )
                        .await,
                    )
                };
                let awaited = match completion {
                    Some(Ok(crate::ToolCompletionEvent::Resolved {
                        position,
                        resolution,
                    })) => {
                        if position >= pending.len() {
                            self.retain_outstanding_group(handle);
                            return Err(crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                                "a completion wait returned an invalid tool position",
                            ));
                        }
                        let (position, completion) = pending.remove(position);
                        let outcome = match self
                            .finish_deferred_tool_completion(*completion, resolution)
                            .await
                        {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                self.retain_outstanding_group(handle);
                                return Err(error);
                            }
                        };
                        let child = GroupChildSettled::Tool(Box::new(outcome));
                        let decides = consumer.decides(child.fulfilled());
                        settled[position] = Some(child);
                        settlement_positions.push(position);
                        if decides {
                            self.incorporate_group_prefix(&handle).await?;
                            self.retain_outstanding_group(handle);
                            return Ok(ToolChildGroupSettled {
                                settled,
                                settlement_positions,

                                cancelled: false,
                            });
                        }
                        continue;
                    }
                    Some(Err(error)) => Err(error),
                    _ => {
                        controller
                            .await_next_settlement(&mut handle, turn_cancel.clone())
                            .await
                    }
                };
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
                    let abandoned = (0..children.len())
                        .filter(|position| {
                            settled[*position].is_none()
                                && matches!(children[*position], PreparedGroupChild::Tool(_))
                        })
                        .collect::<Vec<_>>();
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
                    // finish its drain (§4) and seats its own final, which a
                    // batch surface presents (`present_cancelled_tool_group`).
                    let cursor = crate::EffectGroupHandle::restored(
                        handle.group_key(),
                        handle.children(),
                        handle.consumed(),
                    )?;
                    let group_key = handle.group_key().to_string();
                    if let Err(error) = controller
                        .close_effect_group(handle, LoserPolicy::Cancel)
                        .await
                    {
                        tracing::warn!(
                            error = %error,
                            "closing a cancelled tool-child group failed; the close is retryable"
                        );
                    }
                    self.discharge_abandoned_children(&group_key, children, &abandoned)
                        .await?;
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
                    return Ok(ToolChildGroupSettled {
                        settled,
                        settlement_positions,

                        cancelled: true,
                    });
                }
                Err(error) => {
                    self.retain_outstanding_group(handle);
                    return Err(error);
                }
            };
            let position = settlement.position;
            let child = match self
                .present_group_settlement(
                    handle.group_key(),
                    children,
                    position,
                    settlement.outcome,
                    consume_started,
                )
                .await
            {
                Ok(child) => child,
                Err(error) => {
                    self.retain_outstanding_group(handle);
                    return Err(error);
                }
            };
            if !dispatch_only && let GroupChildSettled::Deferred(completion) = child {
                pending.push((position, completion));
                self.incorporate_group_prefix(&handle).await?;
                continue;
            }
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

                    cancelled: false,
                });
            }
        }
        // Journal and apply the consumed prefix: every settled rank's facts
        // land once under its recorded `child_replay_key`, and a replay
        // re-incorporates exactly this prefix (FIG-3411 part 2).
        self.incorporate_group_prefix(&handle).await?;
        let group_key = handle.group_key().to_owned();
        let past_every_rank = (handle.children() as u64).checked_add(1).ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                "a tool group has more children than ranks",
            )
        })?;
        let closed = controller
            .close_effect_group(handle, LoserPolicy::RunToCompletion)
            .await;
        if dispatch_only {
            closed?;
            // A root waiting phase is quiet only after every dispatch obligation seated.
            controller
                .await_group_child_drain_admission(&group_key, past_every_rank)
                .await?;
        } else if let Err(error) = closed {
            tracing::warn!(error = %error, "closing a consumed tool-child group failed; the disposition is durable and the close is retryable");
        }
        self.release_group_work(&group_key);
        Ok(ToolChildGroupSettled {
            settled,
            settlement_positions,
            cancelled: false,
        })
    }

    /// One settled rank of `group_key`, presented at its group `position`.
    ///
    /// A tool child's recorded outcome becomes its completed call, and a timer
    /// child's `Sleep` becomes a settled timer. A child that refused with a
    /// live fault (`ControllerAborted`: its attempt's journal claim, renew or
    /// finalize failed) is a refusal, not the tool's settlement, even where
    /// the group sealed it as the child's `Failed` terminal: it aborts the
    /// turn as the live fault it is (FIG-3528, FIG-3575). Only a child's
    /// recorded outcome stays on the result surface.
    async fn present_group_settlement(
        &self,
        group_key: &str,
        children: &[PreparedGroupChild],
        position: usize,
        outcome: Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError>,
        consume_started: std::time::Instant,
    ) -> Result<GroupChildSettled, crate::RuntimeEffectControllerError> {
        match (children.get(position), outcome) {
            (
                Some(PreparedGroupChild::Tool(leaf)),
                Ok(crate::RuntimeEffectOutcome::ToolInvocation {
                    outcome,
                    settlement,
                }),
            ) => self
                .apply_tool_child_settlement(
                    &group_child_replay_key(group_key, position),
                    &leaf.call.call,
                    *outcome,
                    *settlement,
                    self.dispatch
                        .clock
                        .now()
                        .saturating_duration_since(consume_started)
                        .as_millis() as u64,
                )
                .await
                .map(|completed| GroupChildSettled::Tool(Box::new(completed))),
            (
                Some(PreparedGroupChild::Tool(_)),
                Ok(crate::RuntimeEffectOutcome::ToolInvocationDeferred { completion }),
            ) => {
                self.emit_recorded_child_stream_value(
                    &group_child_replay_key(group_key, position),
                    &completion.pending.call_id,
                    &serde_json::Value::Null,
                    &completion.stream,
                );
                Ok(GroupChildSettled::Deferred(completion))
            }

            (_, Ok(other)) => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                format!(
                    "durable effect group {group_key} settled position {position} with a {} \
                     outcome, which is not what that child was admitted as",
                    other.kind().as_str(),
                ),
            )),
            (_, Err(mut error)) => {
                if error.code.turn_failure_cause().aborts_invocation() {
                    error.journaled = false;
                }
                Err(error)
            }
        }
    }

    /// Fills every member a cancelled consumer left unconsumed with its
    /// durable final, for a surface that presents one row per member — the
    /// standard protocol's batch and a code executor's tool batch (ADR 0116
    /// §2.6).
    ///
    /// A cancelled rank wait only says the consumer stopped waiting. A member
    /// may already have seated a successful final the wait did not deliver, or
    /// may hold a committed final whose seat is still coming; presenting either
    /// as cancelled would tell the model a tool did not run when it did
    /// (FIG-4364). So after the consumer's close, this waits at the closing
    /// barrier past the last rank until every committed member has seated, as
    /// the opener's end does (§7), then reads each unconsumed rank and presents
    /// it at its position, in rank order: a committed member's recorded result,
    /// and the cancelled reply for each member whose final is the cancel
    /// decision. Rows therefore never disagree with the durable finals.
    ///
    /// Reads only: the group stays the opener's, and its end incorporates
    /// these ranks. Each read is of a seated, immutable rank, so a replay
    /// presents the same rows.
    pub(crate) async fn present_cancelled_tool_group(
        &self,
        group_key: &str,
        children: &[PreparedGroupChild],
        settled: &mut ToolChildGroupSettled,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let controller = self.dispatch.effect_controller.controller();
        let presentation_started = self.dispatch.clock.now();
        // The consumer fills the rank-ordered prefix it consumed, one position
        // per rank, so the next unconsumed rank follows its length.
        let last_rank = u64::try_from(children.len()).ok().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                format!("effect group {group_key} has more children than ranks"),
            )
        })?;
        // Past the last rank: every committed child of the closed group has
        // seated, or retirement released the wait.
        controller
            .await_group_child_drain_admission(group_key, last_rank + 1)
            .await?;
        for rank in 1..=last_rank {
            let ranked = controller
                .read_group_settlement(group_key, rank)
                .await?
                .ok_or_else(|| {
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                        format!(
                            "cancelled effect group {group_key} has no durable final at rank \
                             {rank} after every committed child seated"
                        ),
                    )
                })?;
            if settled.settled.iter().enumerate().any(|(position, child)| {
                child.is_some()
                    && group_child_replay_key(group_key, position) == ranked.child_replay_key
            }) {
                continue;
            }
            let (position, child) = self
                .present_cancelled_group_rank(
                    group_key,
                    children,
                    rank,
                    ranked,
                    presentation_started,
                )
                .await?;
            if settled.settled[position].is_some() {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!("cancelled effect group {group_key} ranked position {position} twice"),
                ));
            }
            settled.settled[position] = Some(child);
            settled.settlement_positions.push(position);
        }
        Ok(())
    }

    /// One unconsumed rank of a cancelled group, read back by
    /// [`Self::present_cancelled_tool_group`]: its position and its
    /// presentation.
    async fn present_cancelled_group_rank(
        &self,
        group_key: &str,
        children: &[PreparedGroupChild],
        rank: u64,
        ranked: crate::runtime::effect::RankedGroupSettlement,
        presentation_started: std::time::Instant,
    ) -> Result<(usize, GroupChildSettled), crate::RuntimeEffectControllerError> {
        let position = (0..children.len())
            .find(|position| {
                group_child_replay_key(group_key, *position) == ranked.child_replay_key
            })
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!(
                        "cancelled effect group {group_key} rank {rank} names child {}, which \
                         is not one of its {} children",
                        ranked.child_replay_key,
                        children.len()
                    ),
                )
            })?;
        match (&children[position], ranked.outcome) {
            (
                PreparedGroupChild::Tool(_),
                Ok(crate::RuntimeEffectOutcome::ToolInvocationDeferred { completion }),
            ) => self
                .cancel_deferred_tool_completion(*completion)
                .await
                .map(|completed| (position, GroupChildSettled::Tool(Box::new(completed)))),
            (PreparedGroupChild::Tool(leaf), Err(error))
                if error.code == crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled =>
            {
                Ok((
                    position,
                    GroupChildSettled::Tool(Box::new(cancelled_group_leaf(leaf))),
                ))
            }

            (_, outcome) => self
                .present_group_settlement(
                    group_key,
                    children,
                    position,
                    outcome,
                    presentation_started,
                )
                .await
                .map(|child| (position, child)),
        }
    }

    /// Drains the cancel obligation of each call this opener's cancel
    /// abandoned (ADR 0116 §3.4). A cancelled child's own invocation may never
    /// reach its park site's discharge, so the opener cancels, from its own
    /// journal, whatever process the call's consumer hold says it owes a
    /// cancel. The call's completion key names the hold: it is derived from
    /// the scope and the call id alone. Abandoning the hold reads what it
    /// owes and refuses any start still racing to register under it; the
    /// answer is recorded, so a replay issues the same cancels after the first
    /// execution's releases emptied the hold.
    async fn discharge_abandoned_children(
        &self,
        group_key: &str,
        children: &[PreparedGroupChild],
        abandoned: &[usize],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let controller = self.dispatch.effect_controller.controller();
        let execution_scope = self.dispatch.effect_controller.execution_scope().clone();
        for &position in abandoned {
            let Some(PreparedGroupChild::Tool(leaf)) = children.get(position) else {
                continue;
            };
            let call_id = &leaf.call.call.call_id;
            let key = match controller
                .await_event_key(
                    &execution_scope,
                    crate::AwaitEventWaitIdentity::tool_completion(call_id.clone()),
                )
                .await
            {
                Ok(key) => key,
                Err(error) => {
                    tracing::warn!(
                        call_id = call_id.as_str(),
                        error = %error,
                        "an abandoned call's completion key could not be derived"
                    );
                    continue;
                }
            };
            let effect_id = group_child_replay_key(group_key, position);
            let owed = match crate::tool_dispatch::consumer_hold_owner(&self.process_scope(None)) {
                Some(owner) => self.recorded_owed_cancels(&effect_id, &key, owner).await?,
                None => Vec::new(),
            };
            let parent = crate::RuntimeInvocation::effect(
                crate::EffectAddress::new(execution_scope.clone(), effect_id.clone())?,
                self.dispatch.parentless_attribution(),
                effect_id,
            );
            let site = crate::tool_dispatch::ParkSite {
                processes: self.dispatch.processes.as_ref(),
                owner: self.dispatch.owner.runtime_owner(),
                call_id,
                scope: self.process_scope(Some(parent)),
                child_trace_hook: None,
            };
            crate::tool_dispatch::discharge_abandoned_call(&site, &key, &owed).await?;
        }
        Ok(())
    }

    /// The processes `key`'s hold owes a cancel, as the first execution read
    /// them: journaled under the abandoned child's effect, and served back on
    /// every replay. Reading them abandons the hold, owned by `owner`, which
    /// refuses any later start under it. An abandonment that fails records
    /// none, leaving the hold to the owning scope's close.
    async fn recorded_owed_cancels(
        &self,
        child_effect: &str,
        key: &crate::AwaitEventKey,
        owner: crate::ScopeId,
    ) -> Result<Vec<crate::ProcessId>, crate::RuntimeEffectControllerError> {
        let processes = Arc::clone(&self.dispatch.processes);
        let key_id = key.key_id.clone();
        let recorded = self
            .journaled_deferred_resolution_with(
                format!("{child_effect}:owed-cancels"),
                "declared-start-owed-cancels".to_string(),
                move || async move {
                    let owed = processes
                        .abandon_consumer_hold(&key_id, &owner)
                        .await
                        .unwrap_or_else(|error| {
                            tracing::warn!(
                                error = %error,
                                "an abandoned call's consumer hold could not be abandoned"
                            );
                            Vec::new()
                        });
                    Ok(serde_json::json!(owed))
                },
            )
            .await?;
        serde_json::from_value(recorded).map_err(|error| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RecordEncodingFailed,
                format!("an abandoned call's recorded cancel obligation does not decode: {error}"),
            )
        })
    }

    /// FIG-3411 seam: returns one settled child's completed call and emits its
    /// activity events.
    ///
    /// The once-only channels — possession, committed checkpoint messages and
    /// trigger receipts (ADR 0099 §6) — are incorporated by the
    /// consumer through `incorporate_group_prefix`, which journals the
    /// `IncorporateGroupSettlements` record covering the consumed prefix
    /// (FIG-3411 part 2). Presentation is taken verbatim from
    /// `settlement.model_return` — the child's own recorded projection, run
    /// once inside the child — rather than re-running the projector. Realized
    /// intent outcomes and their activity events ride the carried
    /// `ToolDispatchOutcome`.
    ///
    /// `call_key` is the child's own effect-invocation replay key —
    /// `{group}:child:{position}` — under which its settlement's observation
    /// lanes emit (ADR 0105 §1). `duration_ms` is the observed window the
    /// opener measured for the child — it rides the Completed activity only;
    /// the journaled outcome holds no wall-clock fields (FIG-3696).
    ///
    /// Deliberately absent here, and owned by FIG-3411's remaining steps:
    /// cross-invocation carriage of the settlement facts (ADR 0099 §8).
    pub(crate) async fn apply_tool_child_settlement(
        &self,
        call_key: &str,
        call: &crate::PreparedToolCall,
        outcome: ToolDispatchOutcome,
        settlement: crate::runtime::ToolSettlement,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        let call_id = call.call_id.clone();
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
        self.emit_recorded_child_stream(call_key, &call_id, &outcome.record, &settlement.stream);
        {
            let context = self.with_call_observation_key(self.call_observation_key(call_key));
            let mut cursor = context.observation_cursor(&format!("tool:{call_id}:intents"));
            // A child's realized outcomes ride its settlement, where its
            // driver moved them; a declared start's launch receipt is one.
            for intent_outcome in &settlement.intent_outcomes {
                cursor.observe(
                    context.dispatch.observer.as_ref(),
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
            call_id: call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool: outcome.record.tool.clone(),
            args: outcome.record.args.clone(),
            output: outcome.record.output.clone(),
        };
        self.emit_tool_call_completed(
            call_key,
            &record,
            &outcome.attempts,
            duration_ms,
            &settlement.intent_outcomes,
        )
        .await;
        Ok(CompletedProtocolToolCall {
            completed: crate::sansio::CompletedToolCall {
                call_id,
                provider_call_id: call.provider_call_id.clone(),
                tool_name: outcome.record.tool,
                args: outcome.record.args,
                output: outcome.record.output,
                model_return: settlement.model_return,
                intent_outcomes: outcome.intent_outcomes,
                replay: call.replay.clone(),
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
    /// each member's durable final — its result if it committed, a cancelled
    /// completion if the cancel decided it — not an error.
    pub async fn dispatch_prepared_tool_group(
        &self,
        batch_id: crate::BatchId,
        group_invocation: crate::RuntimeEffectInvocation,
        prepared_entries: Vec<(usize, crate::PreparedToolCall)>,
    ) -> Result<Vec<(usize, ToolDispatchResult)>, crate::RuntimeEffectControllerError> {
        let batch = crate::PreparedToolBatch::new(
            batch_id.clone(),
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
                    owner: self
                        .dispatch
                        .plugins
                        .tool_execution_owner(&manifest.id, None)
                        .map_err(crate::RuntimeEffectControllerError::from)?,
                    manifest: Box::new(manifest),
                },
            })));
        }
        let consumer = ToolAggregateConsumer::AllSettled;
        let group_key = self.tool_child_group_key(&batch_id);
        let handle = match self
            .open_tool_child_group(
                group_invocation,
                group_key.clone(),
                &batch_id,
                &leaves,
                consumer.wake(),
                crate::GroupReopen::RetainedShape,
            )
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                // The step asked for more tool calls than the session's
                // recorded `max_tool_calls` admits (FIG-4546): every call of
                // it answers the typed refusal, so the model reads the limit
                // in its tool results. Nothing was journaled or dispatched,
                // and a replay refuses the same step the same way.
                let Some(exceeded) = error.tool_call_limit_exceeded() else {
                    return Err(error);
                };
                return Ok(leaves
                    .iter()
                    .filter_map(PreparedGroupChild::tool)
                    .map(|leaf| {
                        (
                            leaf.input_index,
                            ToolDispatchResult::Done(Box::new(limit_refused_group_leaf(
                                leaf, exceeded,
                            ))),
                        )
                    })
                    .collect());
            }
        };
        let mut settled = self
            .consume_tool_child_group_mode(handle, &leaves, consumer, true)
            .await?;
        if settled.cancelled {
            self.present_cancelled_tool_group(&group_key, &leaves, &mut settled)
                .await?;
        }
        let mut results = Vec::with_capacity(leaves.len());
        for (position, leaf) in leaves.iter().enumerate() {
            let leaf = leaf.tool().ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    "a tool dispatch group held a timer",
                )
            })?;
            let result = match settled.settled[position].take() {
                Some(GroupChildSettled::Tool(completed)) => ToolDispatchResult::Done(completed),
                Some(GroupChildSettled::Deferred(completion)) => {
                    ToolDispatchResult::Deferred(completion)
                }
                _ => {
                    return Err(crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                        "a tool dispatch group left a slot empty",
                    ));
                }
            };
            results.push((leaf.input_index, result));
        }
        Ok(results)
    }
}

/// A group child's own invocation replay key, `{group}:child:{position}`: the
/// effect address formation mints for it, and the `child_replay_key` its
/// durable final names.
fn group_child_replay_key(group_key: &str, position: usize) -> String {
    format!(
        "{group_key}:{}",
        crate::runtime::causal::CommandReplayKey::child_suffix(position)
    )
}

/// The presentation of a tool child whose durable final is the cancel
/// decision: the batch surface's cancelled reply (batch.rs's `"tool call
/// cancelled"` shape) as a `CompletedProtocolToolCall`.
fn cancelled_group_leaf(leaf: &PreparedToolChildLeaf) -> CompletedProtocolToolCall {
    let completed = cancelled_completed_tool_call(
        crate::tool_dispatch::ToolCallIds::of(&leaf.call.call),
        leaf.call.call.tool_name.clone(),
        leaf.call.call.args.clone(),
        leaf.call.call.replay.clone(),
    );
    let record = ToolCallRecord {
        call_id: completed.call_id.clone(),
        provider_call_id: completed.provider_call_id.clone(),
        tool: completed.tool_name.clone(),
        args: completed.args.clone(),
        output: completed.output.clone(),
    };
    CompletedProtocolToolCall { completed, record }
}

/// The tool failure a call refused by the session's `max_tool_calls` settles
/// with: typed by its code, never retried, and worded by the refusal so the
/// limit is named wherever the failure is shown (FIG-4546).
pub(crate) fn tool_call_limit_failure(exceeded: crate::ToolCallLimitExceeded) -> ToolFailure {
    ToolFailure::runtime(
        ToolFailureClass::ResourceLimit,
        crate::ToolCallLimitExceeded::CODE,
        exceeded.to_string(),
    )
}

/// The presentation of a tool call its step's `max_tool_calls` refused.
fn limit_refused_group_leaf(
    leaf: &PreparedToolChildLeaf,
    exceeded: crate::ToolCallLimitExceeded,
) -> CompletedProtocolToolCall {
    let ids = crate::tool_dispatch::ToolCallIds::of(&leaf.call.call);
    let tool = leaf.call.call.tool_name.clone();
    let output = ToolCallOutput::failure(tool_call_limit_failure(exceeded));
    let record = ToolCallRecord {
        call_id: ids.call_id.clone(),
        provider_call_id: ids.provider_call_id.clone(),
        tool: tool.clone(),
        args: leaf.call.call.args.clone(),
        output: output.clone(),
    };
    CompletedProtocolToolCall {
        completed: crate::sansio::CompletedToolCall {
            model_return: ModelToolReturn::from_output(tool.clone(), &output),
            call_id: ids.call_id,
            provider_call_id: ids.provider_call_id,
            tool_name: tool,
            args: leaf.call.call.args.clone(),
            output,
            intent_outcomes: Vec::new(),
            replay: leaf.call.call.replay.clone(),
        },
        record,
    }
}

/// What a group tool child records of its opener and emits back to it
/// (FIG-3712).
impl RuntimeExecutionContext<'_> {
    /// Emits the stream events a group child recorded because no opener was
    /// live where it ran (FIG-3712): its session events publish through
    /// `ObservedEvent::Session`, so they project at emission exactly as the
    /// live opener's forwarder projected them, and its turn activities publish
    /// verbatim — they carry the identities the child's recorder minted and
    /// are not re-keyed. Both lanes key under the child's own replay key
    /// (`call_key`). A stream the recording budget cut says so on the session
    /// stream, as a `child_stream_truncated` message.
    fn emit_recorded_child_stream(
        &self,
        call_key: &str,
        call_id: &crate::ToolCallId,
        record: &crate::ToolCallRecord,
        stream: &crate::runtime::effect::AttemptStream,
    ) {
        let record = serde_json::to_value(record).unwrap_or_default();
        self.emit_recorded_child_stream_value(call_key, call_id, &record, stream);
    }

    fn emit_recorded_child_stream_value(
        &self,
        call_key: &str,
        call_id: &crate::ToolCallId,
        record: &serde_json::Value,
        stream: &crate::runtime::effect::AttemptStream,
    ) {
        let (events, undecodable) = stream.decode(record);
        if undecodable > 0 {
            tracing::warn!(
                call_id = call_id.as_str(),
                undecodable,
                "a tool child's recorded stream held events this build cannot decode; \
                 they are skipped"
            );
        }
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor("stream");
        let observer = context.dispatch.observer.as_ref();
        for event in events {
            match event {
                crate::runtime::effect::DecodedStreamEvent::Session(event) => {
                    cursor.observe(observer, crate::engine::ObservedEvent::Session(event));
                }
                crate::runtime::effect::DecodedStreamEvent::Activity(activity) => {
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
                    kind: crate::StreamMessageKind::ChildStreamTruncated,
                }),
            );
        }
    }

    /// The session facts a group tool child this context opens records
    /// (FIG-3712): the tool surface its calls are admitted against, the
    /// session's tool access and subagent context, and which of this
    /// context's sources have no recorded form, and whether its opener had a
    /// context to lend (`opener_context`, FIG-4590).
    pub(crate) fn tool_child_session_facts(
        &self,
        opener_context: crate::runtime::effect::ToolChildOpenerContext,
    ) -> crate::runtime::effect::ToolChildSessionFacts {
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
            subagent: plugins.subagent_context(),
            unrecorded: self.unrecorded_sources.union(
                crate::runtime::effect::UnrecordedSessionSources {
                    fork_plugins: plugins.forked_plugins(),
                    plugin_state: plugins.holds_plugin_state(),
                    ..Default::default()
                },
            ),
            opener_context,
        }
    }
}

/// The refusal a formation meets when a call's recorded request differs
/// from the one it formed under the same id (ADR 0117 §7).
fn retained_request_drift(call: &crate::PreparedToolCall) -> crate::RuntimeEffectControllerError {
    let provider = call
        .provider_call_id
        .as_deref()
        .map(|provider_call_id| format!("provider call `{provider_call_id}`, "))
        .unwrap_or_default();
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::LashlangCellBindingDrift,
        format!(
            "tool call `{}` ({provider}tool `{}`) was recorded with a different tool, \
             arguments or authority than this redrive formed; its journal binds the call \
             to the recorded request, so nothing was dispatched",
            call.call_id, call.tool_id,
        ),
    )
}

/// The digest a call's retained request is bound to: its identity, canonical
/// tool, arguments and authority (ADR 0117 §7). The prepared payload is
/// retained beside it and served, not compared.
fn retained_request_digest(
    call: &crate::PreparedToolCall,
    admission: &ToolChildAdmission,
) -> String {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.tool-call-request",
        TOOL_CALL_REQUEST_FAMILY_VERSION,
    );
    identity.string(call.call_id.as_str());
    identity.string(call.tool_id.as_str());
    identity.string(&call.tool_name);
    identity.bytes(&crate::identity_json::payload_leaf(&call.args));
    match admission {
        ToolChildAdmission::Catalog { manifest, .. } => {
            identity.tag(0);
            identity.string(manifest.id.as_str());
        }
        ToolChildAdmission::Granted { grant } => {
            identity.tag(1);
            identity.string(grant.manifest.id.as_str());
            identity.optional(grant.source_id.as_deref(), |identity, source_id| {
                identity.string(source_id);
            });
            identity.bytes(&crate::identity_json::payload_leaf(
                &grant.execution_binding,
            ));
        }
    }
    crate::stable_identity::rendered_hash(
        "tool-call-request",
        TOOL_CALL_REQUEST_FAMILY_VERSION,
        &identity.finish(),
    )
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "dispatch", rename_all = "snake_case")]
pub enum ToolDispatchResult {
    Done(Box<CompletedProtocolToolCall>),
    Deferred(Box<crate::tool_dispatch::DeferredToolCompletion>),
}
