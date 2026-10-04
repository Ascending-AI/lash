//! The durable controller operations independent of the SDK context shape.

use super::*;

pub trait RestateControllerContext<'ctx>: GroupChildCancelRace<'ctx> + Send + Sync + 'ctx {
    /// The physical journal's engine identity, stable on replay and distinct
    /// for each successor segment. Logical effect identities survive a segment.
    fn invocation_id(&self) -> &str;

    fn sleep_send<'run>(&'run self, duration: Duration) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run;

    /// Register a timer before awaiting any pending attempt on replay.
    fn start_sleep_send<'run>(&'run self, duration: Duration) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run,
    {
        self.sleep_send(duration)
    }

    /// A durable timer, raced against the turn's cancellation gate when
    /// `turn_cancel` names one. A sleep that observes no turn races the
    /// process segment's durable cancel promise when `process_cancel` says
    /// the wait belongs to a process drive (FIG-3673); nothing live ever
    /// races it.
    fn sleep_or_turn_cancel<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run;

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> crate::JournaledFuture<'run, Json<T>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run;

    /// Runs one journaled `ctx.run` step whose fault is never recorded
    /// (ADR 0105 §1: an engine fault is not a domain outcome).
    ///
    /// An `Ok` value is journaled and replayed like
    /// [`run_json_send`](Self::run_json_send)'s. An `Err` ends this attempt
    /// retryably and writes nothing: the step carries no retry policy of its
    /// own, so the engine's invocation retry replays the journal up to it and
    /// runs it again (FIG-3683). The fault's text is all the engine keeps of
    /// the attempt, and nothing after the step runs in it: the returned
    /// future never resolves to the fault.
    /// Register an owned X before waiting; SDK progress owns its closure.
    fn run_json_eager_or_retry_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        future: Fut,
    ) -> impl Future<Output = Result<Json<T>, TerminalError>> + Send + 'run
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = Result<T, String>> + Send + 'run;

    fn run_json_or_retry_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        future: Fut,
    ) -> impl Future<Output = Result<Json<T>, TerminalError>> + Send + 'run
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = Result<T, String>> + Send + 'run;

    /// A captured turn sleep raced against cancellation and generation drain.
    fn sleep_or_turn_end<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        duration: Duration,
        turn_cancel: RestateDurableWaitAwaitRequest,
        generation: lash_core::engine::BuildGeneration,
    ) -> TurnCancelRaceFuture<'run, TurnSleepOutcome>
    where
        'ctx: 'run,
    {
        let _ = generation;
        Box::pin(async move {
            self.sleep_or_turn_cancel(
                namespace,
                duration,
                Some(turn_cancel),
                ProcessCancelRace::NotRaced,
            )
            .await
            .map(|outcome| outcome.map(|()| TurnSleepOutcome::Resolved))
        })
    }

    /// Submits the process's workflow run.
    ///
    /// The failure is classified because the scheduling boundary compensates on
    /// one class and not the other: see [`ProcessWorkflowStartFailure`].
    fn start_process_workflow<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        process_id: lash_core::ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        sender_generation: lash_core::engine::BuildGeneration,
    ) -> crate::JournaledFuture<'run, String, ProcessWorkflowStartFailure>
    where
        'ctx: 'run;

    fn request_process_workflow_cancel<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateProcessCancelRequest,
    ) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run;

    fn await_event<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> crate::JournaledFuture<'run, Resolution>
    where
        'ctx: 'run;

    /// A durable await, raced against the turn's cancellation gate when
    /// `turn_cancel` names one. An await that observes no turn (a process
    /// body's `waitSignal`) races the process segment's durable cancel
    /// promise when `process_cancel` says so (FIG-3673); a lost event wait is
    /// released `Cancelled`.
    fn await_event_or_turn_cancel<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run;

    /// A turn's durable await its Run's successor segment may take over
    /// (FIG-4739): the event raced against the turn's cancellation gate,
    /// with the gate entry registered for the drain of `generation`, the
    /// build generation the turn runs on. A drain wake answers
    /// [`TurnWaitOutcome::HandedOver`] and retires only this physical read,
    /// leaving the event key open for the successor; a
    /// cancel releases it, as [`Self::await_event_or_turn_cancel`] does.
    ///
    /// A context whose waits take no drain wake races the gate alone.
    fn await_event_or_turn_end<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        turn_cancel: RestateDurableWaitAwaitRequest,
        generation: lash_core::engine::BuildGeneration,
    ) -> TurnCancelRaceFuture<'run, TurnWaitOutcome>
    where
        'ctx: 'run,
    {
        let _ = generation;
        Box::pin(segment_wait::turn_cancel_only(
            self,
            namespace,
            request,
            replay_key,
            turn_cancel,
        ))
    }

    run_source_defaults!('ctx);

    fn arm_tool_completion<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _key: lash_core::AwaitEventKey,
    ) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("tool completion arming is unavailable")) })
    }

    fn await_tool_completions<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _waits: Vec<lash_core::ToolCompletionWait>,
        _dispatch: Option<lash_core::ToolDispatchCursor>,
        _turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        _generation: Option<lash_core::engine::BuildGeneration>,
        _process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, lash_core::ToolCompletionEvent>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("tool completion waiting is unavailable")) })
    }

    /// A process segment's signal wait, raced against its cancel and
    /// hand-over promises (FIG-3799): see the `segment_wait` module.
    fn await_signal_or_segment_end<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        generation: lash_core::engine::BuildGeneration,
    ) -> TurnCancelRaceFuture<'run, SignalWaitOutcome>
    where
        'ctx: 'run,
    {
        let _ = generation;
        Box::pin(segment_wait::cancel_only(
            self, namespace, request, replay_key,
        ))
    }

    /// A journaled peek of the running process workflow's own cancellation
    /// promise (FIG-3149, FIG-3673): a process drive's cancel checkpoint and
    /// its post-runner verdict. Every redrive observes exactly the verdict the
    /// first execution committed instead of re-reading live registry state
    /// that can answer differently on replay.
    ///
    /// Contexts without a workflow promise surface carry no process
    /// cancellation and answer `false`.
    fn peek_process_cancel_requested<'run>(&'run self) -> crate::JournaledFuture<'run, bool>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(false) })
    }

    fn peek_event<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        address: RestateDurableWaitAddress,
        replay_key: String,
    ) -> crate::JournaledFuture<'run, Option<Resolution>>
    where
        'ctx: 'run;

    /// A turn cancellation gate's journaled peek: the session's revocation
    /// and the gate's terminal (FIG-3978). A context with a durable-wait
    /// index reads both from the index's shared `peek_turn_gate` in one
    /// call; this default composes the two separate reads.
    fn peek_turn_gate<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        key: lash_core::AwaitEventKey,
    ) -> crate::JournaledFuture<'run, RestateTurnGatePeek>
    where
        'ctx: 'run,
    {
        Box::pin(async move {
            if let Some(session_id) = key.scope.session_id()
                && self
                    .session_is_revoked(namespace, session_id.clone())
                    .await?
            {
                return Ok(RestateTurnGatePeek::Revoked);
            }
            let resolution = self
                .peek_event(
                    namespace,
                    RestateDurableWaitAddress::for_key(&key),
                    key.key_id.clone(),
                )
                .await?;
            Ok(RestateTurnGatePeek::Open(resolution))
        })
    }

    fn resolve_event<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveEventFuture<'run>
    where
        'ctx: 'run;

    /// Send one resolve to the key's index and return without waiting for it
    /// (FIG-3978). This default resolves in place; a context that can send
    /// journals the resolve as a one-way call instead.
    fn publish_event<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitResolveRequest,
    ) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run,
    {
        Box::pin(async move {
            self.resolve_event(namespace, request).await?;
            Ok(())
        })
    }

    /// Hand one process terminal wait to the attach workflow and return without
    /// waiting for it.
    ///
    /// Record a short process-terminal subscription before returning to the
    /// Run's wait. No separate invocation waits on the process terminal.
    fn attach_process_terminal<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateProcessTerminalRequest,
    ) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run;

    fn update_session_waits<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        session_id: SessionId,
        revoke: bool,
    ) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run;

    fn session_is_revoked<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _session_id: SessionId,
    ) -> crate::JournaledFuture<'run, bool>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(false) })
    }

    /// Record an effect starting under the non-session scope whose index is
    /// `index_key`, answering whether the scope admits it (FIG-2499). A
    /// context without a durable-wait index admits everything and records
    /// nothing.
    fn scope_effect_begin<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _index_key: String,
        _replay_key: String,
    ) -> crate::JournaledFuture<'run, bool>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(true) })
    }

    fn scope_effect_end<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _index_key: String,
        _replay_key: String,
    ) -> crate::JournaledFuture<'run, ()>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(()) })
    }

    /// Record an effect group opened under the non-session scope whose index
    /// is `index_key`, answering whether the scope admits it (FIG-2499).
    fn scope_group_record<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _index_key: String,
        _group_key: String,
    ) -> crate::JournaledFuture<'run, bool>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(true) })
    }

    fn effect_group_probe<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
    ) -> crate::JournaledFuture<'run, EffectGroupProbeResponse>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/probe is not registered",
            ))
        })
    }

    fn effect_group_preflight<'run>(
        &'run self,
        _group_key: String,
        _children: Vec<lash_core::RuntimeEffectEnvelope>,
        _route: String,
    ) -> crate::JournaledFuture<'run, Option<usize>>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupDispatch/preflight is not registered",
            ))
        })
    }

    fn effect_group_open<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
        _request: EffectGroupOpenRequest,
    ) -> crate::JournaledFuture<'run, EffectGroupOpenResponse>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/open is not registered",
            ))
        })
    }

    fn effect_group_submit<'run>(
        &'run self,
        _request: EffectGroupDispatchRequest,
        _route: String,
    ) -> crate::JournaledFuture<'run, String>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupDispatch/run is not registered",
            ))
        })
    }

    fn effect_group_read_rank<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
        _request: EffectGroupReadRankRequest,
    ) -> crate::JournaledFuture<'run, EffectGroupReadRankResponse>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/read_rank is not registered",
            ))
        })
    }

    fn effect_group_payload_get<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _payload_key: String,
    ) -> crate::JournaledFuture<'run, EffectGroupPayloadGetResponse>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupPayload/get is not registered",
            ))
        })
    }

    fn effect_group_close<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
        _request: EffectGroupCloseRequest,
    ) -> crate::JournaledFuture<'run, EffectGroupCloseResponse>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/close is not registered",
            ))
        })
    }

    /// The group one replay key is a committed member of under the scope
    /// whose index is `index_key`: the membership record a §4 boundary
    /// resolves before it can name its index (FIG-3409).
    fn scope_group_child_membership<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _index_key: String,
        _replay_key: String,
    ) -> crate::JournaledFuture<'run, Option<String>>
    where
        'ctx: 'run,
    {
        unregistered_group_index("LashDurableWaitIndex/group_child_membership")
    }

    /// The §4 boundary decision for one group child's final record.
    fn effect_group_commit_child<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
        _request: EffectGroupCommitChildRequest,
    ) -> crate::JournaledFuture<'run, EffectGroupCommitChildResponse>
    where
        'ctx: 'run,
    {
        unregistered_group_index("EffectGroupIndex/commit_child")
    }

    /// §4's admission fence for one recorded group child (FIG-3470): the
    /// index's serialized answer to "may a semantic effect minted under this
    /// child still be admitted".
    fn effect_group_admit_semantic<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
        _request: EffectGroupAdmitSemanticRequest,
    ) -> crate::JournaledFuture<'run, EffectGroupAdmitSemanticResponse>
    where
        'ctx: 'run,
    {
        unregistered_group_index("EffectGroupIndex/admit_semantic")
    }

    /// A child's cancel fact as its group index records it: the
    /// step-boundary read of a group child (FIG-4344).
    fn effect_group_child_cancel<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
        _position: usize,
    ) -> crate::JournaledFuture<'run, Option<EffectGroupNotification>>
    where
        'ctx: 'run,
    {
        unregistered_group_index("EffectGroupIndex/child_cancel")
    }

    /// Await one of an effect group's own notices (FIG-4344): an awakeable of
    /// this journal, subscribed at the group index, which answers an
    /// already-true notice at once and otherwise completes the awakeable
    /// itself. A wait on a turn races the turn's cancellation gate when
    /// `turn_cancel` names one, exactly as a durable wait does (FIG-3672 P9);
    /// a wait that observes no turn (a process body's rank wait) races the
    /// process segment's durable cancel promise when `process_cancel` says
    /// so (FIG-3673). A subscriber whose race the other arm won is dropped
    /// with a one-way unsubscribe.
    fn await_effect_group_notice<'run>(
        &'run self,
        _namespace: &'run crate::RestateNamespace,
        _group_key: String,
        _notice: EffectGroupNotice,
        _turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        _process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, EffectGroupNotification>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/subscribe is not registered",
            ))
        })
    }
}
