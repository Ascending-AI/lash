//! One tool call run in memory: its admission checks (A), its attempts (X),
//! its decision (D), its declarations and its presentation (V).
//!
//! Nothing here records anything. A call's durability is the admitted
//! execution that runs it (ADR 0132 §5): a turn's round member, or a code
//! cell's call (§8), whose `x_start` commits before this runs and whose
//! `x_outcome` commits what it answers. A crash between those commits
//! re-runs nothing for a `Once` execution, which is `Interrupted`, and
//! reruns a `Repeatable` one from its admission at the same ordinal. No
//! code here is ever run again against a recorded history.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::singleton_run::{
    BeforeCheckReply, IsolatedBinding, IsolatedProcessDescriptor, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonPreparedRequest, SingletonPresentationError,
    SingletonRunError, SingletonToolCall, SingletonToolHandlers, StartLaunch,
};
use crate::runtime::actor::round::StoreLocalEffect;
use crate::runtime::effect::AttemptStreamRecorder;
use crate::runtime::process::{
    DeclaredStartObligation, DeclaredStartObligationRefusal, DeclaredStartPhase,
    IsolatedStartRefusal, StartCancelDecision,
};
use crate::tool_run::{
    AdmissionRefusal, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict, CallDecision,
    CheckRank, CheckRecord, DeclarationRefusal, ExecutionPolicy, ExternalCancelPolicy, HookCause,
    OutcomeShape, RankedVerdict, ResultSource,
};
use crate::{ConsumerHold, ProcessId, ProcessStartRegistration, ScopeId, ToolCallId};
use lash_sansio::ToolIntentKind;

#[cfg(test)]
#[path = "call_run/tests.rs"]
mod tests;

impl RankedVerdict for BeforeCheckReply {
    fn rank(&self) -> CheckRank {
        match self {
            Self::Allow => CheckRank::Allow,
            Self::Cached { .. } => CheckRank::CachedSuccess,
            Self::Deny { .. } | Self::Cancel { .. } => CheckRank::DenyOrCancel,
            Self::AbortRun { .. } => CheckRank::AbortRun,
        }
    }
}

/// How a call ended: what its consumer, or its round member, presents.
#[derive(Clone, Debug)]
pub enum CallEnd {
    /// The result is final: its declarations realized, presented and
    /// incorporated.
    Final {
        /// What the result was.
        capture: SingletonCapture,
        /// Its model-facing presentation.
        presentation: String,
        /// The process its declared start launches.
        launched: Option<ProcessId>,
        /// The store-local effects its realization and its declared start
        /// staged: they commit with the call's outcome.
        store_local: Vec<StoreLocalEffect>,
    },
    /// A check or the Run's cancel withheld the result.
    Withheld {
        /// Why.
        decision: CallDecision,
        /// The check that decided it, when one did.
        cause: Option<AttributedVerdict<HookCause>>,
    },
}

impl CallEnd {
    /// Whether the call's consumer takes it as a fulfilled value: a final
    /// success whose declarations all realized.
    #[must_use]
    pub fn fulfilled(&self) -> bool {
        matches!(
            self,
            Self::Final {
                capture: SingletonCapture::Done { .. } | SingletonCapture::Isolated { .. },
                ..
            }
        )
    }
}

/// What one attempt left.
#[derive(Clone, Debug)]
pub enum AttemptEnd {
    /// The call ended.
    Ended(CallEnd),
    /// The attempt reported a failure its policy repeats: the next attempt
    /// may follow after its backoff.
    Retry {
        /// The failure the attempt captured, undecided and unpresented.
        capture: SingletonCapture,
        /// The body's own backoff hint.
        suggested_delay_ms: Option<u64>,
    },
    /// The body parked on the completion wait its round pinned: the call
    /// ends when that wait, or the process its resolver awaits, does.
    Parked {
        /// Its pending completion.
        completion: Box<crate::PendingCompletion>,
        /// The launch receipt of the start it declared to resolve it.
        launch: Option<Box<super::LaunchReceipt>>,
        /// That start's store-local effect: it commits with the park.
        store_local: Vec<StoreLocalEffect>,
    },
}

/// What one execution of the body left.
enum Executed {
    /// A captured answer.
    Captured(SingletonCapture),
    /// The body parked.
    Parked {
        completion: Box<crate::PendingCompletion>,
        launch: Option<Box<super::LaunchReceipt>>,
        store_local: Vec<StoreLocalEffect>,
    },
}

/// What admission selected from the call's before-checks.
#[derive(Clone, Debug)]
enum Selection {
    Execute,
    Cached(SingletonCapture),
    Withheld {
        decision: CallDecision,
        cause: AttributedVerdict<HookCause>,
    },
}

/// One tool call, admitted: its prepared request and what its before-checks
/// selected. Its attempts run from here.
pub struct AdmittedToolCall<'a> {
    handlers: Arc<dyn SingletonToolHandlers + 'a>,
    call: SingletonToolCall,
    request: SingletonPreparedRequest,
    selection: Selection,
    policy: ExecutionPolicy,
    address: Option<crate::EffectAddress>,
}

fn fault(message: impl std::fmt::Display) -> SingletonRunError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::EngineEffectController,
        message.to_string(),
    )
    .into()
}

pub(crate) fn start_hold_key(call_id: &ToolCallId) -> String {
    format!("{call_id}:start")
}

/// Bind a call's declared start to the Run: its environment, and a consumer
/// hold carrying the call's cancel policy.
pub(crate) fn bind_start(
    call: &SingletonToolCall,
    cancels: bool,
    mut registration: ProcessStartRegistration,
) -> Result<DeclaredStartObligation, DeclaredStartObligationRefusal> {
    registration.env_ref = call.environment.clone();
    registration.consumer_hold = Some(ConsumerHold {
        key: start_hold_key(&call.call_id),
        owner: ScopeId::Opener(call.owner.clone()),
        cancels,
    });
    DeclaredStartObligation::new(call.call_id.clone(), registration)
}

fn require_isolated_engine(
    handlers: &dyn SingletonToolHandlers,
    kind: &str,
) -> Result<(), IsolatedStartRefusal> {
    handlers
        .process_engines()
        .and_then(|engines| engines.require(kind).ok())
        .map(|_| ())
        .ok_or_else(|| IsolatedStartRefusal::Unavailable {
            kind: kind.to_owned(),
        })
}

/// The binding of an isolated call to its registered process engine.
fn isolated_binding(
    handlers: &dyn SingletonToolHandlers,
    call: &SingletonToolCall,
) -> Result<IsolatedBinding, SingletonRunError> {
    let start = handlers
        .isolated_start(call)
        .ok_or(AdmissionRefusal::UnsupportedIsolation { member: 0 })?;
    let Some(crate::ProcessInput::Engine { kind, .. }) = start.registration.input.input() else {
        return Err(IsolatedStartRefusal::NotEngine.into());
    };
    let engine_kind = kind.clone();
    require_isolated_engine(handlers, &engine_kind)?;
    let obligation = bind_start(
        call,
        call.cancel == ExternalCancelPolicy::CancelExternalWork,
        start.registration,
    )
    .map_err(|cause| IsolatedStartRefusal::Start { cause })?;
    Ok(IsolatedBinding {
        implementation: call.binding.executable.clone(),
        engine_kind,
        obligation: Arc::new(obligation),
    })
}

impl<'a> AdmittedToolCall<'a> {
    /// Admit `call` (A): its declaration, binding and isolation route are
    /// checked, its request prepared once and every before-check asked of
    /// that one prepared request. Nothing executes.
    ///
    /// # Errors
    ///
    /// The call's typed admission refusal, or a handler fault.
    pub async fn admit(
        handlers: Arc<dyn SingletonToolHandlers + 'a>,
        call: SingletonToolCall,
        scope: &crate::ExecutionScope,
    ) -> Result<Self, SingletonRunError> {
        call.declaration
            .validate()
            .map_err(|cause| AdmissionRefusal::Declaration { member: 0, cause })?;
        call.binding
            .require_available(&call.available)
            .map_err(|cause| AdmissionRefusal::BindingUnavailable {
                member: 0,
                cause: Box::new(cause),
            })?;
        let isolation = if call.declaration.isolated {
            Some(isolated_binding(handlers.as_ref(), &call)?)
        } else {
            None
        };
        let state_snapshot = handlers.plugin_session().map(|plugins| {
            Arc::new(
                plugins
                    .export_state()
                    .plugins
                    .get(&call.binding.executable.owner.plugin)
                    .cloned()
                    .unwrap_or_default(),
            )
        });
        let prepared = handlers.prepare(&call).await.map_err(fault)?;
        let request = SingletonPreparedRequest {
            arguments: call.arguments.clone(),
            environment: call.environment.clone(),
            prepared,
            state_snapshot,
            isolation,
        };
        handlers.observe_started(&call.call_id, &request);
        let checks = CheckRecord::reduce(
            handlers
                .before_checks(&call, &request)
                .await
                .map_err(fault)?,
        );
        let selection = match checks.winner() {
            None
            | Some(AttributedVerdict {
                verdict: BeforeCheckReply::Allow,
                ..
            }) => Selection::Execute,
            Some(AttributedVerdict {
                verdict: BeforeCheckReply::Cached { output },
                ..
            }) => Selection::Cached(if call.declaration.isolated {
                SingletonCapture::Refused {
                    refusal: DeclarationRefusal::InlineOutcomeFromIsolated,
                }
            } else {
                handlers.cached_capture(output.clone()).map_err(fault)?
            }),
            Some(AttributedVerdict { callback, verdict }) => {
                let (decision, cause) = match verdict {
                    BeforeCheckReply::Deny { cause } => (CallDecision::Denied, cause),
                    BeforeCheckReply::Cancel { cause } => (CallDecision::CheckCancelled, cause),
                    BeforeCheckReply::AbortRun { cause } => (CallDecision::Aborted, cause),
                    BeforeCheckReply::Allow | BeforeCheckReply::Cached { .. } => {
                        unreachable!("matched above")
                    }
                };
                Selection::Withheld {
                    decision,
                    cause: AttributedVerdict {
                        callback: callback.clone(),
                        verdict: cause.clone(),
                    },
                }
            }
        };
        let policy = handlers.execution_policy(&call);
        let address =
            crate::EffectAddress::new(scope.clone(), format!("lash:call:{}:decide", call.call_id))
                .ok();
        Ok(Self {
            handlers,
            call,
            request,
            selection,
            policy,
            address,
        })
    }

    /// The call.
    #[must_use]
    pub fn call(&self) -> &SingletonToolCall {
        &self.call
    }

    /// The policy its admission sealed.
    #[must_use]
    pub fn policy(&self) -> ExecutionPolicy {
        self.policy
    }

    /// Run attempt `ordinal` to the call's end, or to a failure its policy
    /// repeats when `may_retry` (the attempt is not the last one it admits).
    /// `cancel` is the Run's or the turn's cancel: a result decided after it
    /// fired is withheld as `Cancelled`.
    ///
    /// # Errors
    ///
    /// A handler fault: nothing of the attempt is answered.
    pub async fn attempt(
        &self,
        ordinal: AttemptOrdinal,
        may_retry: bool,
        cancel: &CancellationToken,
    ) -> Result<AttemptEnd, SingletonRunError> {
        let (source, capture) = match &self.selection {
            Selection::Withheld { decision, cause } => {
                return self
                    .withheld(decision.clone(), Some(cause.clone()), None)
                    .map(AttemptEnd::Ended);
            }
            Selection::Cached(capture) => (ResultSource::Cached, capture.clone()),
            Selection::Execute => {
                let capture = match self.execute(ordinal).await? {
                    Executed::Captured(capture) => capture,
                    Executed::Parked {
                        completion,
                        launch,
                        store_local,
                    } => {
                        return Ok(AttemptEnd::Parked {
                            completion,
                            launch,
                            store_local,
                        });
                    }
                };
                if let SingletonCapture::Failed {
                    suggested_delay_ms, ..
                } = &capture
                    && may_retry
                    && self.policy.permits_repeat(self.policy, ordinal.get())
                {
                    let suggested_delay_ms = *suggested_delay_ms;
                    return Ok(AttemptEnd::Retry {
                        capture,
                        suggested_delay_ms,
                    });
                }
                (ResultSource::Attempt { attempt: ordinal }, capture)
            }
        };
        self.decide(source, capture, cancel)
            .await
            .map(AttemptEnd::Ended)
    }

    /// Execute the body once (X) and capture what it answered under the
    /// call's admitted declaration.
    async fn execute(&self, ordinal: AttemptOrdinal) -> Result<Executed, SingletonRunError> {
        if let Some(binding) = &self.request.isolation {
            return Ok(Executed::Captured(SingletonCapture::Isolated {
                binding: Box::new(binding.clone()),
            }));
        }
        let recorder = AttemptStreamRecorder::start();
        let outcome = self
            .handlers
            .execute(SingletonAttempt {
                call_id: &self.call.call_id,
                attempt: ordinal,
                request: &self.request,
                stream: &recorder,
            })
            .await
            .map_err(fault)?;
        let stream = recorder.finish();
        let declaration = &self.call.declaration;
        Ok(Executed::Captured(match outcome {
            SingletonBodyOutcome::Done {
                output,
                commands,
                intents,
                start,
            } => {
                // A declared start is a StartProcess intent of the result.
                let mut declared = intents.clone();
                if start.is_some() && !declared.contains(&ToolIntentKind::StartProcess) {
                    declared.push(ToolIntentKind::StartProcess);
                }
                match declaration.admits(OutcomeShape::Done { intents: &declared }) {
                    Err(refusal) => SingletonCapture::Refused { refusal },
                    Ok(()) => match start.map(|start| {
                        bind_start(
                            &self.call,
                            self.call.cancel == ExternalCancelPolicy::CancelExternalWork,
                            *start,
                        )
                    }) {
                        Some(Err(refusal)) => SingletonCapture::StartRefused { refusal },
                        start => SingletonCapture::Done {
                            output,
                            commands: commands.into_commands(),
                            intents,
                            stream,
                            start: start.and_then(Result::ok).map(Arc::new),
                        },
                    },
                }
            }
            SingletonBodyOutcome::Failed {
                output,
                suggested_delay_ms,
            } => SingletonCapture::Failed {
                output,
                stream,
                suggested_delay_ms,
            },
            SingletonBodyOutcome::Interrupted => SingletonCapture::Interrupted,
            SingletonBodyOutcome::TimedOut { cause, evidence } => {
                SingletonCapture::TimedOut { cause, evidence }
            }
            SingletonBodyOutcome::Cancelled { evidence } => {
                SingletonCapture::Cancelled { evidence }
            }
            SingletonBodyOutcome::Pending {
                completion,
                launch,
                store_local,
            } => {
                return Ok(Executed::Parked {
                    completion,
                    launch,
                    store_local,
                });
            }
        }))
    }

    /// Decide the call (D) on `capture`: the Run's cancel, then every
    /// after-check, with the result's state commands proposed and published
    /// with a final. A final then realizes its declarations and is presented.
    async fn decide(
        &self,
        source: ResultSource,
        capture: SingletonCapture,
        cancel: &CancellationToken,
    ) -> Result<CallEnd, SingletonRunError> {
        let handlers = self.handlers.as_ref();
        let call_id = &self.call.call_id;
        let plugins = handlers.plugin_session();
        let decide = async {
            if cancel.is_cancelled() {
                return Ok((CallDecision::Cancelled, None));
            }
            if let SingletonCapture::Done { commands, .. } = &capture
                && !commands.is_empty()
            {
                let plugins = plugins
                    .as_ref()
                    .ok_or("state commands require a plugin session")?;
                let origin = match &source {
                    ResultSource::Attempt { attempt } => {
                        crate::tool_run::StateCommandOrigin::ToolAttempt {
                            call_id: call_id.clone(),
                            attempt: *attempt,
                        }
                    }
                    _ => crate::tool_run::StateCommandOrigin::ToolAttempt {
                        call_id: call_id.clone(),
                        attempt: AttemptOrdinal::FIRST,
                    },
                };
                crate::plugin::propose(
                    plugins,
                    crate::plugin::Proposal::for_tool(
                        self.call.binding.executable.owner.clone(),
                        origin,
                        commands.clone().into(),
                    ),
                )
                .map_err(|error| error.to_string())?;
            }
            let after = CheckRecord::reduce(handlers.after_checks(call_id, &capture).await?);
            let decision = match after.winner().map(|reply| &reply.verdict) {
                None | Some(AfterCheckVerdict::Allow) => CallDecision::Final {
                    source: source.clone(),
                    declares: capture.declares(),
                },
                Some(AfterCheckVerdict::Deny { .. }) => CallDecision::Denied,
                Some(AfterCheckVerdict::Cancel { .. }) => CallDecision::CheckCancelled,
                Some(AfterCheckVerdict::AbortRun { .. }) => CallDecision::Aborted,
            };
            Ok::<_, String>((decision, Some(after)))
        };
        let (decision, after) = match (&plugins, &self.address) {
            (Some(plugins), Some(address)) => {
                let publication =
                    crate::plugin::EffectPublication::begin(Arc::clone(plugins), address.clone());
                let segment = plugins.state_segment();
                let (decided, proposals) = crate::plugin::collect_proposals(plugins, decide).await;
                let decided = decided.map_err(fault)?;
                if matches!(decided.0, CallDecision::Final { .. })
                    && matches!(
                        capture,
                        SingletonCapture::Done { .. } | SingletonCapture::Isolated { .. }
                    )
                {
                    let state = plugins
                        .reduce_proposals(address, segment, proposals)
                        .await
                        .map_err(fault)?;
                    publication.publish_run(state)?;
                }
                decided
            }
            _ => decide.await.map_err(fault)?,
        };
        if !matches!(decision, CallDecision::Final { .. }) {
            let cause = after
                .as_ref()
                .and_then(CheckRecord::winner)
                .and_then(|reply| match &reply.verdict {
                    AfterCheckVerdict::Deny { cause }
                    | AfterCheckVerdict::Cancel { cause }
                    | AfterCheckVerdict::AbortRun { cause } => Some(AttributedVerdict {
                        callback: reply.callback.clone(),
                        verdict: cause.clone(),
                    }),
                    AfterCheckVerdict::Allow => None,
                });
            return self.withheld(decision, cause, Some(&capture));
        }
        self.present(decision, capture, cancel).await
    }

    /// A withheld call's end: its stream, incorporation and observation.
    fn withheld(
        &self,
        decision: CallDecision,
        cause: Option<AttributedVerdict<HookCause>>,
        capture: Option<&SingletonCapture>,
    ) -> Result<CallEnd, SingletonRunError> {
        let handlers = self.handlers.as_ref();
        let call_id = &self.call.call_id;
        if let Some(stream) = capture.and_then(SingletonCapture::stream) {
            handlers.emit_stream(call_id, stream);
        }
        handlers.incorporate(call_id, capture, None, true)?;
        handlers.observe_terminal(call_id, &decision, cause.as_ref(), capture, None)?;
        Ok(CallEnd::Withheld { decision, cause })
    }

    /// Realize a final's declarations, then present and incorporate it (V).
    /// What the declarations write to the lash store is staged: it commits
    /// with the call's outcome.
    async fn present(
        &self,
        decision: CallDecision,
        capture: SingletonCapture,
        cancel: &CancellationToken,
    ) -> Result<CallEnd, SingletonRunError> {
        let handlers = self.handlers.as_ref();
        let call_id = &self.call.call_id;
        let mut store_local = Vec::new();
        if !capture.intents().is_empty() {
            let realization = handlers.realize(call_id, &capture).await?;
            handlers.adopt_realization(call_id, &realization.receipt)?;
            store_local.extend(realization.store_local);
        }
        let launched = match capture.start() {
            Some(obligation) => {
                let (process_id, effect) = self.launch(obligation, cancel).await?;
                store_local.extend(effect);
                Some(process_id)
            }
            None => None,
        };
        let presentation = match (&capture, &launched) {
            (SingletonCapture::Isolated { binding }, Some(process_id)) => {
                serde_json::to_string(&IsolatedProcessDescriptor {
                    process_id: process_id.clone(),
                    start_key: binding.obligation.start_key().clone(),
                })
                .map_err(fault)?
            }
            _ => match handlers.present(call_id, &capture).await {
                Ok(text) => text,
                // A declared refusal presents the original result.
                Err(SingletonPresentationError::Refused { .. }) => {
                    capture.output().map(str::to_owned).unwrap_or_default()
                }
                Err(SingletonPresentationError::Fault { message }) => return Err(fault(message)),
            },
        };
        if let Some(stream) = capture.stream() {
            handlers.emit_stream(call_id, stream);
        }
        handlers.incorporate(call_id, Some(&capture), Some(&presentation), true)?;
        handlers.observe_terminal(
            call_id,
            &decision,
            None,
            Some(&capture),
            Some(&presentation),
        )?;
        Ok(CallEnd::Final {
            capture,
            presentation,
            launched,
            store_local,
        })
    }

    /// Stage a final's declared start under its key: the process it
    /// launches, and the rows that register it with the call's outcome. A
    /// cancel of the Run that fired by now launches nothing when the call's
    /// policy owes the start a cancel.
    async fn launch(
        &self,
        obligation: &DeclaredStartObligation,
        cancel: &CancellationToken,
    ) -> Result<(ProcessId, Option<StoreLocalEffect>), SingletonRunError> {
        if cancel.is_cancelled()
            && matches!(
                obligation.on_cancel(DeclaredStartPhase::Launched),
                StartCancelDecision::RecoverAndDischarge {
                    cancel_process: true,
                    ..
                }
            )
        {
            return Err(fault(format!(
                "the Run was cancelled before call {}'s declared start launched",
                self.call.call_id
            )));
        }
        match self.handlers.stage_start(obligation).await.map_err(fault)? {
            StartLaunch::Staged { handle, effect } => Ok((handle.process_id, effect)),
            StartLaunch::Refused(refusal) => Err(fault(refusal.describe())),
        }
    }

    /// Discharge the call's external work when its Run closes before it
    /// ended: its handler cancels what its policy owes a cancel.
    ///
    /// # Errors
    ///
    /// A handler fault.
    pub async fn cancel_external_work(&self) -> Result<(), SingletonRunError> {
        if self.call.cancel != ExternalCancelPolicy::CancelExternalWork {
            return Ok(());
        }
        self.handlers
            .cancel_call(&self.call.call_id)
            .await
            .map_err(fault)
    }
}
