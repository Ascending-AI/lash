//! Turn ingress for a child session's in-process turn: accepting it as durable
//! admission evidence and driving the root that admits the row it just wrote
//! (ADR 0069).
//!
//! Everything here runs before the prepare phase and hands it the admitted
//! rows, the drive fence, and the input the admission materialized.

use super::*;
use crate::TurnId;

fn clear_process_invocation_correlation_for_ordinary_turn(
    input: &mut TurnInput,
    execution_scope: &crate::ExecutionScope,
) {
    if !matches!(execution_scope, crate::ExecutionScope::Process { .. }) {
        lash_core_execution::core_internal::clear_process_invocation_correlation(
            &mut input.turn_context,
        );
    }
}

impl LashRuntime {
    pub(in crate::runtime) async fn stream_turn_with_scoped_effect_controller_inner(
        &mut self,
        context: TurnPrepareContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let TurnPrepareContext {
            mut input,
            protocol_turn_options,
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            local_stop,
            admissions,
            materialize_initial_admissions,
            drive_fence,
        } = context;
        input
            .trace_turn_id
            .get_or_insert_with(|| TurnId::from(scoped_effect_controller.scope_id()));
        // The scope identifies the authority that admitted this run. Physical
        // turn ids remain separate routing and trace attribution, including for
        // queued drains, runtime operations, processes, and follow-on frames.
        // Re-scoping a borrowed or shared controller would change only the
        // outer address while leaving the host's inner fence on the admitted
        // scope, so preserve the controller unchanged for the complete run.
        // The stable execution-scope turn id is attached to every write-ahead
        // intent before ingress, tools, plugins, or envelope normalization can
        // put bytes. Replays bind the same id; no live pending-id state is used.
        let _attachment_execution_binding =
            match self.host.core.durability.attachment_store.holder() {
                lash_core_execution::attachments::AttachmentHolder::Runtime(
                    crate::RuntimeOwner::Session(_),
                ) => Some(
                    self.host
                        .core
                        .durability
                        .attachment_store
                        .bind_execution_scoped(
                            scoped_effect_controller
                                .execution_scope()
                                .journal_identity()?,
                        )
                        .map_err(|error| {
                            RuntimeError::new(
                                crate::RuntimeErrorCode::RuntimeEffectAttachmentStore,
                                error.to_string(),
                            )
                        })?,
                ),
                _ => None,
            };
        Box::pin(self.stream_turn_inner(TurnPrepareContext {
            input: input.clone(),
            protocol_turn_options,
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            local_stop: local_stop.clone(),
            admissions,
            materialize_initial_admissions,
            drive_fence,
        }))
        .await
    }

    /// Run one child session's turn inside its parent's execution, following
    /// foreground AgentFrame switches until a terminal outcome is reached.
    ///
    /// A host never drives a turn: it sends an input and the engine's session
    /// drive runs it (FIG-3600). The one turn the kernel drives in process is
    /// a child session's, which runs under its parent's process or turn
    /// controller (`session_init`). The turn is still *accepted* first:
    /// `input`'s durable projection is committed as a `NextTurn` Pending Turn
    /// Input row through a journaled acceptance step
    /// ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)),
    /// whose id derives from the acceptance address and whose source key is
    /// the turn id, so a replay adopts the row the first run wrote. The
    /// session drive body then runs it in arrival order, and the call returns
    /// the run of the root that drove it, with the acceptance identity on
    /// [`AgentFrameRun::acceptance`] and on the admitted turn's
    /// [`AssembledTurn::turn_input_acceptance`]. The live `TurnContext`
    /// (the parent's process correlation and lineage) cannot be persisted, so
    /// it is re-attached when the root's admission drives the row.
    ///
    /// A store-less runtime has no store to accept into and drives `input`
    /// directly.
    pub(crate) async fn stream_turn_with_agent_frames(
        &mut self,
        mut input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        // FIG-3353: an enqueue-only (`PreservePersisted`) open may never run a
        // turn — refuse before the acceptance commit becomes admission
        // evidence.
        self.refuse_turn_execution_on_preserved_tool_surface()?;
        clear_process_invocation_correlation_for_ordinary_turn(
            &mut input,
            opts.scoped_effect_controller().execution_scope(),
        );
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
            return Box::pin(self.drive_logical_turn(
                LogicalTurnStart::Input(input, None),
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                opts.scoped_effect_controller(),
                opts.local_stop().clone(),
                LogicalTurnAdmissions::new(Vec::new(), Vec::new()),
                None,
                stopwatch,
            ))
            .await;
        };

        if let Some(trace_turn_id) = input.trace_turn_id.as_ref()
            && opts
                .scoped_effect_controller()
                .execution_scope()
                .validates_turn_trace_id()
            && trace_turn_id.as_str() != opts.execution_scope_id()
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "input trace_turn_id `{trace_turn_id}` does not match execution scope id `{}`",
                    opts.execution_scope_id()
                ),
            ));
        }
        // FIG-3619: a session whose state generation this build cannot run
        // is refused before its input becomes admission evidence.
        store
            .read_session_state_version()
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        // The turn id names the root the accepted input starts: it is the
        // row's host id, so the drive runs the turn under this id (ADR 0069
        // §6, FIG-3600 ruling Q4).
        let trace_turn_id = input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| TurnId::from(opts.execution_scope_id()));
        input.trace_turn_id = Some(trace_turn_id.clone());
        // Acceptance is journaled, not written directly: it happens before the
        // turn runs, which puts it inside a durable engine's replay window, and
        // a replayed handler must re-derive this admission rather than mint a
        // second one (ADR 0069 §6).
        let scoped_effect_controller = opts.scoped_effect_controller();
        let acceptance_invocation = super::causal::turn_acceptance_effect_invocation(
            scoped_effect_controller.execution_scope(),
            &self.state.session_id,
            &trace_turn_id,
        );
        let accepted = scoped_effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    acceptance_invocation.clone(),
                    crate::RuntimeEffectCommand::AcceptTurnInput {
                        // The id is provisioned from the acceptance address
                        // before the body runs, so a body re-run because its
                        // outcome was never recorded names the row the first
                        // run wrote and the store adopts it (ADR 0069 §6). The
                        // source key is the turn id: the drive runs the row
                        // under it.
                        draft: Box::new(
                            crate::PendingTurnInputDraft::new(
                                self.state.session_id.clone(),
                                crate::TurnInputIngress::next_turn(),
                                input.durable_projection(),
                            )
                            .with_input_id(super::turn_input_ingress::provisioned_turn_input_id(
                                acceptance_invocation.address(),
                            ))
                            .with_source_key(trace_turn_id.as_str()),
                        ),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::turn_acceptance(
                    Arc::clone(store.store()) as Arc<dyn crate::TurnInputStore>
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_accepted_turn_input)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        let acceptance = crate::TurnInputAcceptanceReceipt::from(&accepted);
        // From here the input is durably accepted, so every abort names it: the
        // host withdraws or redrives the input by this receipt (FIG-3575).
        let aborted = |err: RuntimeError| err.with_turn_input_acceptance(acceptance.clone());
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            lash_trace::TraceContext::default()
                .for_session(self.state.session_id.clone())
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                .for_turn_index(self.state.turn_index + 1)
                .for_turn(trace_turn_id.clone()),
            lash_trace::TraceEvent::Custom {
                name: "turn_input.accepted".to_string(),
                payload: serde_json::json!({ "input_id": &accepted.input_id }),
            },
            self.host.core.clock.as_ref(),
        );

        // The accepted row is driven by the session drive, in arrival order:
        // any root admitted ahead of it runs first, and the drive stops once
        // the root that drove this row has run. The request is named by the
        // turn, so a redrive of the turn replays the same admissions.
        let request = crate::engine::DriveRequest {
            session: self.state.session_id.clone(),
            request: crate::engine::DriveRequestId::new(format!("turn:{trace_turn_id}")),
            build_generation: self.host.core.backend().build_generation().clone(),
        };
        let sinks = crate::runtime::drive::DriveSinks {
            events: opts.events_or_noop(),
            turn_events: opts.turn_events_or_noop(),
            local_stop: opts.local_stop().clone(),
            settled: &crate::runtime::drive::NoopRootSettledSink,
        };
        let accepted_id = accepted.input_id.clone();
        // A follow-on the head owes is recovered by the session's drive, not
        // by a direct turn: this drive stops where admission names it, and
        // the accepted row waits behind it (ADR 0101 §3, FIG-3542).
        let crate::runtime::drive::DriveRun {
            outcome,
            runs,
            declined_follow_on,
            ..
        } = Box::pin(self.drive_until(
            &scoped_effect_controller,
            &request,
            &sinks,
            Some((&accepted_id, &input)),
            crate::runtime::drive::DriveLimits {
                follow_on: crate::runtime::drive::FollowOnRecovery::Decline,
                max_roots: None,
            },
            |run| run.driven_inputs.contains(&accepted_id),
        ))
        .await
        .map_err(|abort| aborted(abort.into_error()))?;
        let Some(mut run) = runs
            .into_iter()
            .find(|run| run.driven_inputs.contains(&accepted_id))
            .and_then(|run| run.run)
        else {
            if let crate::engine::DriveStop::Parked(park) = &outcome.stop {
                // A parked root holds the session: the input stays accepted
                // and is driven once the park is resolved.
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::SessionRootPending,
                    format!(
                        "accepted turn input `{accepted_id}` waits behind parked root `{}` \
                         (park {}); it is driven once that park is resolved",
                        park.root, park.park
                    ),
                )));
            }
            // A follow-on the head owes blocks every other admission (ADR 0101
            // §3, FIG-3542), so the accepted row stays pending behind it: no
            // turn runs. The drive that recovers the follow-on answers the row
            // after it; a send's handle waits for that (FIG-3600).
            if let Some(ahead) = Box::pin(self.queued_behind_pending_follow_on(
                &store,
                &accepted_id,
                declined_follow_on,
            ))
            .await
            .map_err(aborted)?
            {
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::SessionRootPending,
                    format!(
                        "accepted turn input `{accepted_id}` waits behind the follow-on the \
                         session head owes, with {ahead} earlier inputs ahead of it; the drive \
                         that recovers the follow-on answers it"
                    ),
                )));
            }
            return Err(aborted(RuntimeError::new(
                RuntimeErrorCode::AcceptedTurnInputCeded,
                format!(
                    "accepted turn input `{accepted_id}` was no longer open when the drive \
                     reached it: another driver settled it or the host cancelled it"
                ),
            )));
        };
        // Only the physical turn this acceptance admitted carries it. An
        // agent-frame run's follow-on turns were started by the frame switch,
        // not by this admission, and stamping them would report an acceptance
        // that never applied to them.
        if let Some(admitted) = run.turns.first_mut() {
            admitted.turn_input_acceptance = Some(acceptance.clone());
        }
        run.acceptance = Some(acceptance);
        Ok(run)
    }

    /// How many accepted rows wait ahead of `accepted_id` when a follow-on
    /// held it back, or `None` when nothing did: the row is no longer
    /// pending, or no follow-on was owed. A follow-on held it back when the
    /// drive `declined` the recovery its admission named, or when the
    /// refreshed head still owes one.
    async fn queued_behind_pending_follow_on(
        &self,
        store: &crate::store::SessionStore,
        accepted_id: &crate::InputId,
        declined: bool,
    ) -> Result<Option<u64>, RuntimeError> {
        if !declined && self.state.pending_follow_on.is_none() {
            return Ok(None);
        }
        let open = store
            .list_pending_turn_inputs()
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(own) = open.iter().find(|read| read.input.input_id == *accepted_id) else {
            return Ok(None);
        };
        if !matches!(own.status, crate::PendingTurnInputReadStatus::Open) {
            return Ok(None);
        }
        let ahead = open
            .iter()
            .filter(|earlier| {
                earlier.input.state == crate::TurnInputState::DeferredNextTurn
                    && earlier.input.enqueue_seq < own.input.enqueue_seq
            })
            .count();
        Ok(Some(u64::try_from(ahead).unwrap_or(u64::MAX)))
    }
}

#[cfg(test)]
mod process_invocation_correlation_tests {
    use super::*;

    fn invocation_id(input: &TurnInput) -> Option<String> {
        crate::testing::TestExecutionContextBuilder::over_controller(std::sync::Arc::new(
            crate::testing::UnavailableEffectController,
        )
            as std::sync::Arc<dyn crate::RuntimeEffectController>)
        .turn_context(input.turn_context.clone())
        .build()
        .into_runtime()
        .engine_execution_id()
        .map(str::to_owned)
    }

    fn correlated_input() -> TurnInput {
        let process_id = crate::ProcessId::fixture("process:subagent:call");
        let authority = crate::ProcessExecutionWriteAuthority::invocation(
            process_id.clone(),
            "invocation:subagent:call",
        )
        .bind_attempt(2);
        let mut input = TurnInput::text("run child");
        lash_core_execution::core_internal::attach_process_invocation_correlation(
            &mut input.turn_context,
            &process_id,
            &authority,
        );
        input
    }

    #[test]
    fn ordinary_turn_clears_reused_process_invocation_correlation() {
        let mut input = correlated_input();
        assert_eq!(
            invocation_id(&input).as_deref(),
            Some("invocation:subagent:call")
        );

        clear_process_invocation_correlation_for_ordinary_turn(
            &mut input,
            &crate::ExecutionScope::turn("session:child", "turn:follow-up"),
        );

        assert_eq!(invocation_id(&input), None);
    }

    #[test]
    fn process_turn_preserves_attached_invocation_correlation() {
        let mut input = correlated_input();

        clear_process_invocation_correlation_for_ordinary_turn(
            &mut input,
            &crate::ExecutionScope::process(crate::ProcessId::fixture("process:subagent:call")),
        );

        assert_eq!(
            invocation_id(&input).as_deref(),
            Some("invocation:subagent:call")
        );
    }
}
