//! The Run coordinator (K3, FIG-4877, FIG-4880): the calls of one logical Run
//! record their admission (A), attempt (X), decision (D), declarations and
//! presentation with incorporation (V) in the Run's opener journal, and the
//! Run drains every committed final's protected work in rank order.
//!
//! Calls are admitted individually or as an ordered round. Each decision
//! takes the Run's next rank, so ranks follow the order decisions became durable. A final
//! whose result declares Lash intents owes protected work: its declarations
//! are issued only after its decision is durable and only once every
//! committed final ranked below it is seated, and they settle before its
//! presentation. An intent-free final seats at its decision without waiting,
//! so its seat certifies nothing about the ranks below it: the drain frontier
//! is every lower rank, never only the one just below (L18). Presentation and
//! incorporation follow in rank order, so the incorporated calls are always a
//! rank prefix.
//!
//! The Run's cancellation is read only inside a decision's step. A call
//! decided before it stays final: its declarations, presentation and
//! incorporation still drain. A call decided after it is cancelled and
//! declares nothing. A Deferred attempt takes no rank; the source's seal
//! finishes its call, and until then it holds neither a value nor a place in
//! the drain.
//!
//! Records are appended one at a time, in program order, so a replay serves
//! them in the order they were recorded. Concurrent attempts and their
//! recorded schedule use independent owned handles (FIG-4879). While a final's declarations drain, any
//! effect the caller issued before the drain keeps progressing; nothing of
//! the Run's waits on it.
//!
//! The bounded stream a body emits is part of its attempt's capture (X) and
//! is emitted when the Run presents the call (V): no tool-child settlement
//! carries it.
//!
//! A final's declared process start (K5, FIG-4884) is protected work of the
//! same drain: admitted in its `declare` record, registered under its key
//! (`start:launch`) and discharged (`start:discharge`) before its
//! presentation settles the declarations.

use std::collections::BTreeMap;

mod deferred;
mod parallel;

use lash_sansio::ToolCallId;

use lash_sansio::ToolIntentKind;

use super::singleton_run::{
    BeforeCheckReply, IsolatedProcessDescriptor, RecordedIsolatedStart, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonDrift, SingletonPreparedRequest,
    SingletonRunError, SingletonStart, SingletonTerminal, SingletonToolCall, SingletonToolHandlers,
};
use crate::runtime::effect::{AttemptStreamRecorder, ScopedEffectController};
use crate::runtime::process::{
    DeclaredStartObligation, DeclaredStartObligationRefusal, DeclaredStartPhase,
    IsolatedStartRefusal, IsolatedToolStart, ProcessExecutionBoundary, StartCancelDecision,
    WorkerTerminationReceipt,
};
use crate::store::plugin_writers::PluginRevision;
use crate::tool_run::{
    AdmissionRefusal, AdmittedCall, AfterCheckVerdict, AttemptOrdinal, AttemptResult,
    AttributedVerdict, BeforeCheckVerdict, BeforeSelection, CallDecision, CheckRecord,
    DeclarationRefusal, ExternalCancelPolicy, MaterialEntry, MaterialLocation, MaterialOwner,
    MaterialPayload, MaterialRef, MaterialRefusal, MaterialRole, OutcomeShape, ResultSource,
    RoundAdmission, RunEvent, RunEventRefusal, RunJournalEntry, RunLedger, RunRecord,
    RuntimeCallPolicy, SegmentOrdinal,
};
use crate::{
    AwaitEventKey, ConsumerHold, EffectOpener, ProcessId, ProcessStartRegistration,
    RuntimeEffectControllerError, ScopeId,
};

/// The canonical material the served records of one Run own.
struct Materials {
    owner: MaterialOwner,
    available: Vec<PluginRevision>,
    entries: BTreeMap<MaterialRef, Option<MaterialPayload>>,
}

/// Canonical material `owner` owns, journal-local to its Run, and the entry
/// its record carries.
fn mint(
    owner: &MaterialOwner,
    role: MaterialRole,
    text: String,
) -> Result<(MaterialRef, MaterialEntry), String> {
    let payload = MaterialPayload::new(owner.clone(), role, None, text);
    let reference = payload
        .reference(MaterialLocation::JournalLocal)
        .map_err(|error| error.to_string())?;
    Ok((
        reference.clone(),
        MaterialEntry::Available {
            reference,
            payload: Box::new(payload),
        },
    ))
}

impl Materials {
    /// Admit a served entry's material, each payload checked against its
    /// reference before anything reads it.
    fn admit(&mut self, materials: Vec<MaterialEntry>) -> Result<(), RuntimeEffectControllerError> {
        for material in materials {
            match material {
                MaterialEntry::Available { reference, payload } => {
                    payload.verify(&reference, &self.owner, &self.available)?;
                    self.entries.insert(reference, Some(*payload));
                }
                MaterialEntry::Retired { reference } => {
                    self.entries.insert(reference, None);
                }
            }
        }
        Ok(())
    }

    fn read(&self, reference: &MaterialRef) -> Result<&str, RuntimeEffectControllerError> {
        match self.entries.get(reference) {
            Some(Some(payload)) => {
                payload.verify(reference, &reference.owner, &self.available)?;
                Ok(&payload.text)
            }
            Some(None) => Err(MaterialRefusal::Retired {
                reference: Box::new(reference.clone()),
            }
            .into()),
            None => Err(MaterialRefusal::Missing {
                reference: Box::new(reference.clone()),
            }
            .into()),
        }
    }

    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        reference: &MaterialRef,
    ) -> Result<T, RuntimeEffectControllerError> {
        serde_json::from_str(self.read(reference)?).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EffectReplayDivergence,
                format!(
                    "recorded material {} does not decode: {error}",
                    reference.digest
                ),
            )
        })
    }
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

/// The fold and the material of one Run as its records are served.
struct RunJournal<'a> {
    scoped: &'a ScopedEffectController<'a>,
    owner: EffectOpener,
    segment: SegmentOrdinal,
    ledger: RunLedger,
    materials: Materials,
    records: Vec<RunRecord>,
    entries: Vec<RunJournalEntry>,
}

impl RunJournal<'_> {
    fn record(&self, events: Vec<RunEvent>) -> RunRecord {
        RunRecord {
            segment: self.segment,
            first: self.ledger.next_ordinal(),
            events,
            trace: None,
        }
    }

    /// Journal one record, or serve the recorded one, and admit it through
    /// the fold and the material check before anything acts on it.
    async fn append_entry(
        &mut self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<(RunRecord, Vec<MaterialRef>), SingletonRunError> {
        self.scoped.admit_journal_write()?;
        let entry = self
            .scoped
            .controller()
            .record_run_record(name, step)
            .await?;
        let references = entry
            .materials
            .iter()
            .map(|material| match material {
                MaterialEntry::Available { reference, .. }
                | MaterialEntry::Retired { reference } => reference.clone(),
            })
            .collect();
        Ok((self.accept(entry)?, references))
    }

    fn accept(&mut self, entry: RunJournalEntry) -> Result<RunRecord, SingletonRunError> {
        self.ledger.append(self.segment, &entry.record)?;
        self.materials.admit(entry.materials.clone())?;
        self.entries.push(entry.clone());
        self.records.push(entry.record.clone());
        Ok(entry.record)
    }

    async fn append(
        &mut self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<RunRecord, SingletonRunError> {
        Ok(self.append_entry(name, step).await?.0)
    }
}

fn record_name(call_id: &ToolCallId, step: &str) -> String {
    format!("lash:run:{call_id}:{step}")
}

/// Admission's typed refusal of the call itself, checked before the
/// admission record is written so that no refused call is ever recorded.
fn admit_live(
    call: &SingletonToolCall,
    handlers: &dyn SingletonToolHandlers,
    index: usize,
) -> Result<Option<IsolatedToolStart>, SingletonRunError> {
    let index = u32::try_from(index).map_err(|_| boundary(&call.call_id))?;
    call.declaration
        .validate()
        .map_err(|cause| AdmissionRefusal::Declaration {
            member: index,
            cause,
        })?;

    call.binding
        .require_available(&call.available)
        .map_err(|cause| AdmissionRefusal::BindingUnavailable {
            member: index,
            cause: Box::new(cause),
        })?;
    if !call.declaration.isolated {
        return Ok(None);
    }
    let start = handlers
        .isolated_start(call)
        .ok_or(AdmissionRefusal::UnsupportedIsolation { member: index })?;
    let Some(crate::ProcessInput::Engine { kind, .. }) = start.registration.input.input() else {
        return Err(IsolatedStartRefusal::NotEngine.into());
    };
    require_isolated_engine(handlers, kind, start.boundary)?;
    bind_start(
        call,
        &RuntimeCallPolicy {
            cancel: call.cancel,
            ..RuntimeCallPolicy::default()
        },
        start.registration.clone(),
    )
    .map_err(|cause| IsolatedStartRefusal::Start { cause })?;
    Ok(Some(start))
}

fn require_isolated_engine(
    handlers: &dyn SingletonToolHandlers,
    kind: &str,
    boundary: ProcessExecutionBoundary,
) -> Result<std::sync::Arc<dyn crate::ProcessEngine>, IsolatedStartRefusal> {
    let engine = handlers
        .process_engines()
        .and_then(|engines| engines.require(kind).ok())
        .ok_or_else(|| IsolatedStartRefusal::Unavailable {
            kind: kind.to_owned(),
        })?;
    if boundary == ProcessExecutionBoundary::WorkerProcess && engine.physical_worker().is_none() {
        return Err(IsolatedStartRefusal::Boundary {
            kind: kind.to_owned(),
            recorded: boundary,
            available: ProcessExecutionBoundary::Invocation,
        });
    }
    Ok(engine)
}

fn before_verdict(
    owner: &MaterialOwner,
    reply: AttributedVerdict<BeforeCheckReply>,
    minted: &mut Vec<MaterialEntry>,
) -> Result<AttributedVerdict<BeforeCheckVerdict>, String> {
    let verdict = match reply.verdict {
        BeforeCheckReply::Allow => BeforeCheckVerdict::Allow,
        BeforeCheckReply::Cached { output } => {
            let capture = SingletonCapture::Done {
                commands: Vec::new(),
                output,
                intents: Vec::new(),
                stream: crate::runtime::effect::AttemptStream::default(),
                start: None,
            };
            let (result, entry) = mint(owner, MaterialRole::AttemptOutput, encode(&capture)?)?;
            minted.push(entry);
            BeforeCheckVerdict::Cached { result }
        }
        BeforeCheckReply::Deny { cause } => BeforeCheckVerdict::Deny { cause },
        BeforeCheckReply::Cancel { cause } => BeforeCheckVerdict::Cancel { cause },
        BeforeCheckReply::AbortRun { cause } => BeforeCheckVerdict::AbortRun { cause },
    };
    Ok(AttributedVerdict {
        callback: reply.callback,
        verdict,
    })
}

async fn prepare_admitted_call(
    owner: &MaterialOwner,
    call: &SingletonToolCall,
    handlers: &dyn SingletonToolHandlers,
    live_start: Option<IsolatedToolStart>,
    retry: crate::tool_run::RecordedRetryPolicy,
) -> Result<(AdmittedCall, Vec<MaterialEntry>), String> {
    let mut minted = Vec::new();
    let isolation = match live_start {
        None => None,
        Some(start) => {
            let Some(crate::ProcessInput::Engine { kind, .. }) = start.registration.input.input()
            else {
                return Err(IsolatedStartRefusal::NotEngine.to_string());
            };
            let engine_kind = kind.clone();
            let obligation = bind_start(
                call,
                &RuntimeCallPolicy {
                    cancel: call.cancel,
                    ..RuntimeCallPolicy::default()
                },
                start.registration,
            )
            .map_err(|cause| cause.to_string())?;
            let (reference, entry) =
                mint(owner, MaterialRole::PreparedRequest, encode(&obligation)?)?;
            minted.push(entry);
            Some(RecordedIsolatedStart {
                implementation: call.binding.executable.clone(),
                engine_kind,
                boundary: start.boundary,
                start: SingletonStart {
                    start_key: obligation.start_key().clone(),
                    obligation: reference,
                },
            })
        }
    };
    let request = SingletonPreparedRequest {
        arguments: call.arguments.clone(),
        prepared: handlers.prepare(call).await?,
        state_snapshot: handlers.plugin_session().map(|plugins| {
            plugins
                .export_state()
                .plugins
                .get(&call.binding.executable.owner.plugin)
                .cloned()
                .unwrap_or_default()
        }),
        isolation,
    };
    let (request_ref, request_entry) =
        mint(owner, MaterialRole::PreparedRequest, encode(&request)?)?;
    minted.push(request_entry);
    let mut checks = Vec::new();
    for reply in handlers.before_checks(call, &request).await {
        checks.push(before_verdict(owner, reply, &mut minted)?);
    }
    Ok((
        AdmittedCall {
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            request: request_ref,
            declaration: call.declaration.clone(),
            binding: call.binding.clone(),
            policy: RuntimeCallPolicy {
                cancel: call.cancel,
                retry,
            },
            checks: CheckRecord::reduce(checks),
        },
        minted,
    ))
}

fn validate_admitted_call(
    journal: &RunJournal<'_>,
    call: &SingletonToolCall,
    handlers: &dyn SingletonToolHandlers,
    member: AdmittedCall,
    index: usize,
) -> Result<(AdmittedCall, SingletonPreparedRequest), SingletonRunError> {
    let index = u32::try_from(index).map_err(|_| boundary(&call.call_id))?;
    if member.call_id != call.call_id {
        return Err(SingletonRunError::Drift {
            call_id: call.call_id.clone(),
            drift: SingletonDrift::CallId,
        });
    }
    if member.tool_name != call.tool_name {
        return Err(SingletonRunError::Drift {
            call_id: call.call_id.clone(),
            drift: SingletonDrift::ToolName,
        });
    }
    let request: SingletonPreparedRequest = journal.materials.decode(&member.request)?;
    if request.arguments != call.arguments {
        return Err(SingletonRunError::Drift {
            call_id: call.call_id.clone(),
            drift: SingletonDrift::Arguments,
        });
    }
    // The recorded round passes admission against this build's revisions
    // before anything executes; replay never consults the live catalog.
    if member.declaration.isolated {
        let binding = request
            .isolation
            .as_ref()
            .ok_or(AdmissionRefusal::UnsupportedIsolation { member: index })?;
        if binding.implementation != member.binding.executable {
            return Err(SingletonRunError::Drift {
                call_id: call.call_id.clone(),
                drift: SingletonDrift::IsolationBinding,
            });
        }
        require_isolated_engine(handlers, &binding.engine_kind, binding.boundary)?;
        let obligation = recorded_obligation(journal, &call.call_id, &binding.start)?;
        if !matches!(obligation.registration.input.input(), Some(crate::ProcessInput::Engine { kind, .. }) if kind == &binding.engine_kind)
        {
            return Err(IsolatedStartRefusal::NotEngine.into());
        }
    } else if request.isolation.is_some() {
        return Err(IsolatedStartRefusal::NotEngine.into());
    }

    Ok((member, request))
}

struct DecisionSlot {
    record: RunRecord,
    rank: u64,
    address: crate::EffectAddress,
    aborted: bool,
}

async fn decision_entry(
    call: &SingletonToolCall,
    handlers: &dyn SingletonToolHandlers,
    member: &AdmittedCall,
    checked: Option<(ResultSource, SingletonCapture)>,
    slot: DecisionSlot,
) -> Result<RunJournalEntry, String> {
    let DecisionSlot {
        mut record,
        rank,
        address,
        aborted,
    } = slot;
    let protected_source = matches!(&checked, Some((ResultSource::DeferredCompletion { .. }, _)));
    let success = matches!(
        &checked,
        Some((
            _,
            SingletonCapture::Done { .. } | SingletonCapture::Isolated { .. }
        ))
    );
    let plugins = handlers.plugin_session();
    let selection = member.selection();
    let decide = async {
        let (decision, after) = match (selection, checked) {
            _ if aborted && !protected_source => (CallDecision::Cancelled, None),
            (BeforeSelection::Deny, _) => (CallDecision::Denied, None),
            (BeforeSelection::Cancel, _) => (CallDecision::Cancelled, None),
            (BeforeSelection::AbortRun, _) => (CallDecision::Aborted, None),
            (_, None) => return Err("a result candidate has no capture".to_owned()),
            (_, Some(_)) if !protected_source && handlers.run_cancel_requested() => {
                (CallDecision::Cancelled, None)
            }
            (_, Some((source, capture))) => {
                if let SingletonCapture::Done { commands, .. } = &capture
                    && !commands.is_empty()
                {
                    let plugins = handlers
                        .plugin_session()
                        .ok_or("state commands require a plugin session")?;
                    let origin = match &source {
                        ResultSource::DeferredCompletion { attempt, .. } => {
                            crate::tool_run::StateCommandOrigin::DeferredFinalization {
                                call_id: call.call_id.clone(),
                                attempt: *attempt,
                            }
                        }
                        ResultSource::Attempt { attempt } => {
                            crate::tool_run::StateCommandOrigin::ToolAttempt {
                                call_id: call.call_id.clone(),
                                attempt: *attempt,
                            }
                        }
                        _ => crate::tool_run::StateCommandOrigin::ToolAttempt {
                            call_id: call.call_id.clone(),
                            attempt: AttemptOrdinal::FIRST,
                        },
                    };
                    crate::plugin::propose(
                        &plugins,
                        crate::plugin::Proposal::for_tool(
                            member.binding.executable.owner.clone(),
                            origin,
                            commands.clone().into(),
                        ),
                    )
                    .map_err(|error| error.to_string())?;
                }
                let after =
                    CheckRecord::reduce(handlers.after_checks(&call.call_id, &capture).await);
                let decision = match after.winner().map(|reply| &reply.verdict) {
                    None | Some(AfterCheckVerdict::Allow) => CallDecision::Final {
                        source,
                        declares: capture.declares(),
                    },
                    Some(AfterCheckVerdict::Deny { .. }) => CallDecision::Denied,
                    Some(AfterCheckVerdict::Cancel { .. }) => CallDecision::Cancelled,
                    Some(AfterCheckVerdict::AbortRun { .. }) => CallDecision::Aborted,
                };
                (decision, Some(after))
            }
        };
        record.events.push(RunEvent::Decided {
            call_id: call.call_id.clone(),
            rank,
            decision,
            after,
        });
        Ok(RunJournalEntry {
            record,
            materials: Vec::new(),
            state: Vec::new(),
        })
    };
    let Some(plugins) = plugins else {
        return decide.await;
    };
    let (entry, proposals) = crate::plugin::collect_proposals(&plugins, decide).await;
    let mut entry = entry?;
    if success
        && entry.record.events.iter().any(|event| {
            matches!(
                event,
                RunEvent::Decided {
                    decision: CallDecision::Final { .. },
                    ..
                }
            )
        })
    {
        entry.state = plugins
            .reduce_proposals(&address, proposals)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(entry)
}

/// How a call left its decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecidedCall {
    /// The call's one decision is durable under `rank`; its presentation is
    /// owed to the Run's drain.
    Ranked { rank: u64, decision: CallDecision },
    /// The attempt parked on a Deferred source: no decision, no rank, and no
    /// place in the drain until the source's seal finishes the call.
    Deferred { source: AwaitEventKey },
}

enum Handlers<'a> {
    Borrowed(&'a dyn SingletonToolHandlers),
    Owned(std::sync::Arc<dyn SingletonToolHandlers>),
}

impl Handlers<'_> {
    fn get(&self) -> &dyn SingletonToolHandlers {
        match self {
            Self::Borrowed(handlers) => *handlers,
            Self::Owned(handlers) => handlers.as_ref(),
        }
    }
}

/// A decided call whose presentation the drain owes.
struct Owed<'a> {
    call_id: ToolCallId,
    handlers: Handlers<'a>,
    decision: CallDecision,
    /// The result candidate the decision checked, when one existed.
    capture: Option<SingletonCapture>,
}

struct Waiting<'a> {
    call: SingletonToolCall,
    member: AdmittedCall,
    handlers: Handlers<'a>,
    attempt: AttemptOrdinal,
}

/// The calls of one logical Run, recorded in its opener journal.
pub struct RunCoordinator<'a> {
    journal: RunJournal<'a>,
    /// Decided calls whose presentation is owed, by rank.
    owed: BTreeMap<u64, Owed<'a>>,
    pending: Vec<parallel::Pending<'a>>,
    attempts: Vec<crate::tool_run::RunAttemptEntry>,
    cut: Option<crate::tool_run::Cut>,
    faulted: bool,
    active_frame: bool,
    sources: BTreeMap<ToolCallId, crate::tool_run::SourceDescriptor>,
    waiting: BTreeMap<ToolCallId, Waiting<'a>>,
}

impl<'a> RunCoordinator<'a> {
    /// The Run `owner` opens, appended by its active `segment` in the opener
    /// journal `scoped` serves, executing only plugin revisions in
    /// `available`.
    #[must_use]
    pub fn open(
        scoped: &'a ScopedEffectController<'a>,
        owner: EffectOpener,
        segment: SegmentOrdinal,
        available: Vec<PluginRevision>,
    ) -> Self {
        Self {
            journal: RunJournal {
                scoped,
                owner: owner.clone(),
                segment,
                ledger: RunLedger::new(owner.clone()),
                materials: Materials {
                    owner: MaterialOwner::Run { opener: owner },
                    available,
                    entries: BTreeMap::new(),
                },
                records: Vec::new(),
                entries: Vec::new(),
            },
            owed: BTreeMap::new(),
            pending: Vec::new(),
            attempts: Vec::new(),
            cut: None,
            faulted: false,
            active_frame: false,
            sources: BTreeMap::new(),
            waiting: BTreeMap::new(),
        }
    }

    /// The records the Run holds, in journal order.
    #[must_use]
    pub fn records(&self) -> &[RunRecord] {
        &self.journal.records
    }

    #[must_use]
    pub fn into_records(self) -> Vec<RunRecord> {
        self.journal.records
    }

    /// Admit `call` as a singleton round, run its attempt and record its one
    /// decision (A, X, D). Its presentation is owed to [`Self::drain`].
    ///
    /// # Errors
    ///
    /// A typed [`SingletonRunError`]; none of them executes a body.
    pub async fn decide(
        &mut self,
        call: &'a SingletonToolCall,
        handlers: &'a dyn SingletonToolHandlers,
    ) -> Result<DecidedCall, SingletonRunError> {
        self.begin_frame()?;
        let result = self.decide_inner(call, handlers).await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    async fn decide_inner(
        &mut self,
        call: &'a SingletonToolCall,
        handlers: &'a dyn SingletonToolHandlers,
    ) -> Result<DecidedCall, SingletonRunError> {
        let (member, request) = self
            .admit(call, handlers, crate::tool_run::RecordedRetryPolicy::Never)
            .await?;
        let journal = &mut self.journal;

        // The result candidate the decision checks, and where it came from.
        let candidate = match member.selection() {
            BeforeSelection::Execute => {
                match attempt(
                    journal,
                    call,
                    &member,
                    &request,
                    handlers,
                    self.sources.get(&call.call_id).map(|source| &source.source),
                )
                .await?
                {
                    AttemptCaptured::Captured(capture) => {
                        let recorded = match &capture {
                            SingletonCapture::Isolated { binding } => Some(binding.as_ref()),
                            _ => None,
                        };
                        if recorded != request.isolation.as_ref() {
                            return Err(SingletonRunError::Drift {
                                call_id: call.call_id.clone(),
                                drift: SingletonDrift::IsolationBinding,
                            });
                        }
                        Some((
                            ResultSource::Attempt {
                                attempt: AttemptOrdinal::FIRST,
                            },
                            capture,
                        ))
                    }
                    AttemptCaptured::Deferred(source) => {
                        self.waiting.insert(
                            call.call_id.clone(),
                            Waiting {
                                call: call.clone(),
                                member,
                                handlers: Handlers::Borrowed(handlers),
                                attempt: AttemptOrdinal::FIRST,
                            },
                        );
                        return Ok(DecidedCall::Deferred { source });
                    }
                }
            }
            BeforeSelection::Cached => {
                let Some(BeforeCheckVerdict::Cached { result }) =
                    member.checks.winner().map(|reply| &reply.verdict)
                else {
                    return Err(RunEventRefusal::DecisionUnsupported {
                        call_id: call.call_id.clone(),
                    }
                    .into());
                };
                Some((
                    ResultSource::Cached,
                    if member.declaration.isolated {
                        SingletonCapture::Refused {
                            refusal: DeclarationRefusal::InlineOutcomeFromIsolated,
                        }
                    } else {
                        journal.materials.decode(result)?
                    },
                ))
            }
            BeforeSelection::Deny | BeforeSelection::Cancel | BeforeSelection::AbortRun => None,
        };

        self.decide_candidate(call, Handlers::Borrowed(handlers), &member, candidate)
            .await
    }

    async fn admit(
        &mut self,
        call: &SingletonToolCall,
        handlers: &dyn SingletonToolHandlers,
        retry: crate::tool_run::RecordedRetryPolicy,
    ) -> Result<(AdmittedCall, SingletonPreparedRequest), SingletonRunError> {
        self.admit_round(std::slice::from_ref(call), handlers, retry)
            .await?
            .pop()
            .ok_or_else(|| boundary(&call.call_id))
    }

    async fn admit_round(
        &mut self,
        calls: &[SingletonToolCall],
        handlers: &dyn SingletonToolHandlers,
        retry: crate::tool_run::RecordedRetryPolicy,
    ) -> Result<Vec<(AdmittedCall, SingletonPreparedRequest)>, SingletonRunError> {
        if self.faulted {
            return Err(RunCutRefusal::InvocationFailed.into());
        }
        if let Some(cut) = self.cut {
            return Err(RunCutRefusal::AdmissionFrozen { reason: cut.reason }.into());
        }
        let Some(first_call) = calls.first() else {
            return Ok(Vec::new());
        };
        let mut ids = std::collections::BTreeSet::new();
        let mut starts = Vec::with_capacity(calls.len());
        for (index, call) in calls.iter().enumerate() {
            if !ids.insert(&call.call_id) {
                return Err(AdmissionRefusal::DuplicateCall {
                    call_id: call.call_id.clone(),
                }
                .into());
            }
            if self.journal.materials.owner
                != (MaterialOwner::Run {
                    opener: call.owner.clone(),
                })
            {
                return Err(RunEventRefusal::ForeignOwner.into());
            }
            if call.segment != self.journal.segment {
                return Err(RunEventRefusal::NotActiveSegment {
                    active: self.journal.segment.0,
                    found: call.segment.0,
                }
                .into());
            }
            starts.push(admit_live(call, handlers, index)?);
        }
        let journal = &mut self.journal;
        let first = journal.record(Vec::new());
        let owner = journal.materials.owner.clone();
        let admit = Box::pin(async move {
            let mut members = Vec::with_capacity(calls.len());
            let mut materials = Vec::new();
            for (call, start) in calls.iter().zip(starts) {
                let (member, minted) =
                    prepare_admitted_call(&owner, call, handlers, start, retry.clone()).await?;
                members.push(member);
                materials.extend(minted);
            }
            let operands = (0..members.len())
                .map(u32::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            Ok(RunJournalEntry {
                record: RunRecord {
                    events: vec![RunEvent::Admitted {
                        round: RoundAdmission {
                            owner: first_call.owner.clone(),
                            members,
                            operands,
                        },
                    }],
                    ..first
                },
                materials,
                state: Vec::new(),
            })
        });
        let admitted = journal
            .append(record_name(&first_call.call_id, "admit"), admit)
            .await?;
        let Some(RunEvent::Admitted { round }) = admitted.events.first() else {
            return Err(boundary(&first_call.call_id));
        };
        if round.members.len() != calls.len()
            || round.operands
                != (0..calls.len())
                    .map(u32::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| boundary(&first_call.call_id))?
        {
            return Err(boundary(&first_call.call_id));
        }
        let admitted = calls
            .iter()
            .zip(&round.members)
            .enumerate()
            .map(|(index, (call, member))| {
                validate_admitted_call(journal, call, handlers, member.clone(), index)
            })
            .collect::<Result<Vec<_>, _>>()?;
        round
            .clone()
            .admit(&journal.materials.available, |tool_name| {
                admitted.iter().any(|(member, request)| {
                    member.tool_name == tool_name && request.isolation.is_some()
                })
            })?;
        for (call, (member, _)) in calls.iter().zip(&admitted) {
            if member.declaration.may_defer && member.selection() == BeforeSelection::Execute {
                let key = journal
                    .scoped
                    .controller()
                    .await_event_key(
                        call.owner.admitted_scope().scope(),
                        crate::AwaitEventWaitIdentity::tool_completion(call.call_id.clone()),
                    )
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                let descriptor = crate::tool_run::SourceDescriptor {
                    source: key,
                    call_id: call.call_id.clone(),
                    owner: call.owner.clone(),
                    resolver: member.binding.executable.owner.clone(),
                    authority: crate::tool_run::SourceAuthority::ExternalCompletion,
                    cancel: member.policy.cancel,
                };
                journal.scoped.admit_journal_write()?;
                journal
                    .scoped
                    .controller()
                    .arm_run_source(descriptor.clone())
                    .await?;
                self.sources.insert(call.call_id.clone(), descriptor);
            }
        }
        Ok(admitted)
    }

    async fn decide_candidate(
        &mut self,
        call: &SingletonToolCall,
        handlers: Handlers<'a>,
        member: &AdmittedCall,
        candidate: Option<(ResultSource, SingletonCapture)>,
    ) -> Result<DecidedCall, SingletonRunError> {
        let journal = &mut self.journal;
        // D: the one final-or-cancel decision, under the Run's next rank. The
        // Run's cancellation is read here and nowhere else, so the decision
        // chooses once.
        let rank = journal.ledger.next_rank();
        let decide_record = journal.record(Vec::new());
        let checked = candidate.clone();
        let plugins = handlers.get().plugin_session();
        let address = crate::EffectAddress::new(
            journal.scoped.execution_scope().clone(),
            record_name(&call.call_id, "decide"),
        )
        .map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                error.to_string(),
            )
        })?;
        let publication = plugins
            .clone()
            .map(|plugins| crate::plugin::EffectPublication::begin(plugins, address.clone()));
        let decide = Box::pin(decision_entry(
            call,
            handlers.get(),
            member,
            checked,
            DecisionSlot {
                record: decide_record,
                rank,
                address,
                aborted: journal.ledger.aborted(),
            },
        ));
        journal.scoped.admit_journal_write()?;
        let decided_entry = journal
            .scoped
            .controller()
            .record_run_record(record_name(&call.call_id, "decide"), decide)
            .await?;
        let state = decided_entry.state.clone();
        let decided = journal.accept(decided_entry)?;
        if let Some(publication) = publication {
            publication.publish_run(state)?;
        }
        let Some(RunEvent::Decided { rank, decision, .. }) = decided.events.first() else {
            return Err(boundary(&call.call_id));
        };
        self.owed.insert(
            *rank,
            Owed {
                call_id: call.call_id.clone(),
                handlers,
                decision: decision.clone(),
                capture: candidate.map(|(_, capture)| capture),
            },
        );
        Ok(DecidedCall::Ranked {
            rank: *rank,
            decision: decision.clone(),
        })
    }

    /// Drain every decided call in rank order: a final's declarations once
    /// every lower committed final is seated, then its presentation with its
    /// incorporation (V).
    ///
    /// # Errors
    ///
    /// A typed [`SingletonRunError`]. A drain asked of a final whose lower
    /// ranks are not seated refuses with [`RunEventRefusal::DrainFrontier`]
    /// before it issues anything.
    pub async fn drain(
        &mut self,
    ) -> Result<Vec<(ToolCallId, SingletonTerminal)>, SingletonRunError> {
        self.begin_frame()?;
        let result = self.drain_inner().await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    async fn drain_inner(
        &mut self,
    ) -> Result<Vec<(ToolCallId, SingletonTerminal)>, SingletonRunError> {
        let owed = std::mem::take(&mut self.owed);
        let mut terminals = Vec::with_capacity(owed.len());
        for (rank, owed) in owed {
            let call_id = owed.call_id.clone();
            terminals.push((call_id, self.present(rank, owed).await?));
        }
        Ok(terminals)
    }

    async fn present(
        &mut self,
        rank: u64,
        owed: Owed<'a>,
    ) -> Result<SingletonTerminal, SingletonRunError> {
        let Owed {
            call_id,
            handlers,
            decision,
            capture,
        } = owed;
        let handlers = handlers.get();
        let journal = &mut self.journal;
        let (CallDecision::Final { declares, source }, Some(capture)) =
            (&decision, capture.clone())
        else {
            // V: a withheld call is presented by its decision and
            // incorporated; the stream its body emitted is still the host's.
            let present = journal.record(presented(&call_id, None));
            let emitted = capture;
            let step_call = call_id.clone();
            journal
                .append(
                    record_name(&call_id, "present"),
                    Box::pin(async move {
                        if let Some(stream) = emitted.as_ref().and_then(SingletonCapture::stream) {
                            handlers.emit_stream(&step_call, stream);
                        }
                        Ok(RunJournalEntry {
                            state: Vec::new(),
                            record: present,
                            materials: Vec::new(),
                        })
                    }),
                )
                .await?;
            return Ok(SingletonTerminal::Withheld { decision });
        };

        // A final's declarations are issued only after its decision is
        // durable and every lower committed final is seated, and settle
        // before its presentation.
        // Its declared start is admitted with them and drains before they
        // settle.
        let mut settle = Vec::new();
        let mut launched = None;
        let mut termination = None;
        if *declares {
            if !journal.ledger.drain_frontier_open(rank) {
                return Err(RunEventRefusal::DrainFrontier { call_id }.into());
            }
            let obligation = match capture.start() {
                Some(start) => Some(recorded_obligation(journal, &call_id, start)?),
                None => None,
            };
            let mut issue = vec![RunEvent::DeclarationsIssued {
                call_id: call_id.clone(),
            }];
            if let Some(obligation) = &obligation {
                issue.push(RunEvent::StartAdmitted {
                    call_id: call_id.clone(),
                    start_key: obligation.start_key().clone(),
                });
            }
            let issued = journal.record(issue);
            journal
                .append(
                    record_name(&call_id, "declare"),
                    Box::pin(async move {
                        Ok(RunJournalEntry {
                            state: Vec::new(),
                            record: issued,
                            materials: Vec::new(),
                        })
                    }),
                )
                .await?;
            if let Some(obligation) = &obligation {
                let isolated = match &capture {
                    SingletonCapture::Isolated { binding } => Some(binding.as_ref()),
                    _ => None,
                };
                let (process, receipt) =
                    drain_start(journal, &call_id, obligation, isolated, handlers).await?;
                launched = Some(process);
                termination = receipt;
            }
            settle.push(RunEvent::DeclarationsSettled {
                call_id: call_id.clone(),
            });
        }

        // V: presentation, owning only bytes distinct from the output, in one
        // record with its incorporation.
        let present_record = journal.record(Vec::new());
        let owner = journal.materials.owner.clone();
        let final_capture = capture.clone();
        let declares = *declares;
        let step_call = call_id.clone();
        let descriptor = match (&capture, &launched) {
            (SingletonCapture::Isolated { binding }, Some(process_id)) => {
                Some(IsolatedProcessDescriptor {
                    process_id: process_id.clone(),
                    start_key: binding.start.start_key.clone(),
                    boundary: binding.boundary,
                    termination,
                })
            }
            _ => None,
        };
        let present = Box::pin(async move {
            if declares && !final_capture.intents().is_empty() {
                handlers
                    .realize_declarations(&step_call, final_capture.intents())
                    .await?;
            }
            if let Some(stream) = final_capture.stream() {
                handlers.emit_stream(&step_call, stream);
            }
            let text = match descriptor {
                Some(descriptor) => encode(&descriptor)?,
                None => handlers.present(&step_call, &final_capture).await?,
            };
            let mut owned = Vec::new();
            let presentation = if final_capture.output() == Some(text.as_str()) {
                None
            } else {
                let (reference, entry) = mint(&owner, MaterialRole::Presentation, text)?;
                owned.push(entry);
                Some(reference)
            };
            let mut events = settle;
            events.extend(presented(&step_call, presentation));
            Ok(RunJournalEntry {
                state: Vec::new(),
                record: RunRecord {
                    events,
                    ..present_record
                },
                materials: owned,
            })
        });
        let presented_record = journal
            .append(record_name(&call_id, "present"), present)
            .await?;
        let presentation = presented_record
            .events
            .iter()
            .find_map(|event| match event {
                RunEvent::Presented { presentation, .. } => Some(presentation.clone()),
                _ => None,
            });
        let presentation = match presentation.flatten() {
            Some(reference) => journal.materials.read(&reference)?.to_owned(),
            None => capture
                .output()
                .map(str::to_owned)
                .ok_or_else(|| boundary(&call_id))?,
        };
        Ok(SingletonTerminal::Final {
            source: source.clone(),
            capture,
            presentation,
            launched,
        })
    }
}

/// The hold key of a call's declared start: the call's own id, so a call
/// holds at most one process.
fn start_hold_key(call_id: &ToolCallId) -> String {
    format!("{call_id}:start")
}

/// Bind a body's declared start to the Run: the Run's environment, when lash
/// executes the process, and a consumer hold, owned by the Run's opener, that
/// carries the call's recorded cancel policy.
fn bind_start(
    call: &SingletonToolCall,
    policy: &RuntimeCallPolicy,
    mut registration: ProcessStartRegistration,
) -> Result<DeclaredStartObligation, DeclaredStartObligationRefusal> {
    registration.env_ref = if registration.input.is_externally_owned() {
        None
    } else {
        call.environment.clone()
    };
    registration.consumer_hold = Some(ConsumerHold {
        key: start_hold_key(&call.call_id),
        owner: ScopeId::Opener(call.owner.clone()),
        cancels: policy.cancel == ExternalCancelPolicy::CancelExternalWork,
    });
    DeclaredStartObligation::new(call.call_id.clone(), registration)
}

/// The obligation a recorded attempt owns, checked against the key and call
/// the capture names.
fn recorded_obligation(
    journal: &RunJournal<'_>,
    call_id: &ToolCallId,
    start: &SingletonStart,
) -> Result<DeclaredStartObligation, SingletonRunError> {
    let obligation: DeclaredStartObligation = journal.materials.decode(&start.obligation)?;
    if obligation.start_key() != &start.start_key || &obligation.call_id != call_id {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectReplayDivergence,
            format!(
                "call {call_id}'s recorded start obligation does not name start {}",
                start.start_key
            ),
        )
        .into());
    }
    Ok(obligation)
}

/// K5 inside the protected drain: register the admitted start under its key,
/// then discharge it. The Run's cancellation is read once, inside the
/// discharge step, and the recorded cancel policy decides what it does to
/// the launched process.
async fn drain_start(
    journal: &mut RunJournal<'_>,
    call_id: &ToolCallId,
    obligation: &DeclaredStartObligation,
    isolated: Option<&RecordedIsolatedStart>,
    handlers: &dyn SingletonToolHandlers,
) -> Result<(ProcessId, Option<WorkerTerminationReceipt>), SingletonRunError> {
    let start_key = obligation.start_key().clone();
    let launch_record = journal.record(Vec::new());
    let (step_call, key) = (call_id.clone(), start_key.clone());
    let launch = Box::pin(async move {
        let process_id = handlers.launch_start(obligation).await?;
        Ok(RunJournalEntry {
            state: Vec::new(),
            record: RunRecord {
                events: vec![RunEvent::StartLaunched {
                    call_id: step_call,
                    start_key: key,
                    process_id,
                }],
                ..launch_record
            },
            materials: Vec::new(),
        })
    });
    let launched = journal
        .append(record_name(call_id, "start:launch"), launch)
        .await?;
    let Some(RunEvent::StartLaunched { process_id, .. }) = launched.events.first() else {
        return Err(RunEventRefusal::StartOrder {
            call_id: call_id.clone(),
            start_key,
        }
        .into());
    };
    let process_id = process_id.clone();

    let engine = isolated
        .map(|binding| require_isolated_engine(handlers, &binding.engine_kind, binding.boundary))
        .transpose()?;
    let hard =
        isolated.is_some_and(|binding| binding.boundary == ProcessExecutionBoundary::WorkerProcess);
    let owner = journal.materials.owner.clone();
    let discharge_record = journal.record(Vec::new());
    let (step_call, launched_id) = (call_id.clone(), process_id.clone());
    let discharge = Box::pin(async move {
        let cancel = handlers.run_cancel_requested()
            && matches!(
                obligation.on_cancel(DeclaredStartPhase::Launched),
                StartCancelDecision::RecoverAndDischarge {
                    cancel_process: true,
                    ..
                }
            );
        let mut materials = Vec::new();
        if cancel && hard {
            let worker = engine
                .as_ref()
                .and_then(|engine| engine.physical_worker())
                .ok_or("the admitted physical worker is unavailable")?;
            let receipt = worker
                .terminate_worker(&launched_id)
                .await
                .map_err(|error| error.to_string())?;
            if receipt.process_id != launched_id {
                return Err(IsolatedStartRefusal::TerminationOwner.to_string());
            }
            let (_, entry) = mint(&owner, MaterialRole::AttemptOutput, encode(&receipt)?)?;
            materials.push(entry);
        }
        handlers
            .discharge_start(obligation, &launched_id, cancel)
            .await?;
        Ok(RunJournalEntry {
            state: Vec::new(),
            record: RunRecord {
                events: vec![RunEvent::StartDischarged {
                    call_id: step_call,
                    start_key,
                    cancelled: cancel,
                }],
                ..discharge_record
            },
            materials,
        })
    });
    let (record, references) = journal
        .append_entry(record_name(call_id, "start:discharge"), discharge)
        .await?;
    let receipt = match references.first() {
        Some(reference) => {
            let receipt: WorkerTerminationReceipt = journal.materials.decode(reference)?;
            if receipt.process_id != process_id {
                return Err(IsolatedStartRefusal::TerminationOwner.into());
            }
            Some(receipt)
        }
        _ => None,
    };
    if hard
        && receipt.is_none()
        && record.events.iter().any(|event| {
            matches!(
                event,
                RunEvent::StartDischarged {
                    cancelled: true,
                    ..
                }
            )
        })
    {
        return Err(IsolatedStartRefusal::TerminationMissing.into());
    }
    Ok((process_id, receipt))
}

fn boundary(call_id: &ToolCallId) -> SingletonRunError {
    RunEventRefusal::BoundaryOrder {
        call_id: call_id.clone(),
    }
    .into()
}

fn presented(call_id: &ToolCallId, presentation: Option<MaterialRef>) -> Vec<RunEvent> {
    vec![
        RunEvent::Presented {
            call_id: call_id.clone(),
            presentation,
        },
        RunEvent::Consumed {
            call_id: call_id.clone(),
        },
        RunEvent::Incorporated {
            call_id: call_id.clone(),
        },
    ]
}

enum AttemptCaptured {
    Captured(SingletonCapture),
    Deferred(AwaitEventKey),
}

/// X: attempt 1 of the body, checked against the recorded declaration
/// before its record admits anything it declared. The stream the body emits
/// is recorded, bounded, in the same capture.
async fn attempt(
    journal: &mut RunJournal<'_>,
    call: &SingletonToolCall,
    member: &AdmittedCall,
    request: &SingletonPreparedRequest,
    handlers: &dyn SingletonToolHandlers,
    completion_key: Option<&AwaitEventKey>,
) -> Result<AttemptCaptured, SingletonRunError> {
    let record = journal.record(Vec::new());
    let owner = journal.materials.owner.clone();
    let step = Box::pin(async move {
        let captured = capture_attempt(
            owner,
            call,
            member,
            request,
            handlers,
            AttemptOrdinal::FIRST,
            completion_key,
        )
        .await?;
        let result = captured.result;
        let materials = captured.materials;
        Ok(RunJournalEntry {
            state: Vec::new(),
            record: RunRecord {
                events: vec![RunEvent::AttemptRecorded {
                    call_id: call.call_id.clone(),
                    attempt: AttemptOrdinal::FIRST,
                    result,
                }],
                ..record
            },
            materials,
        })
    });
    journal.scoped.admit_journal_write()?;
    let entry = journal
        .scoped
        .controller()
        .record_run_record(record_name(&call.call_id, "attempt:1"), step)
        .await?;
    let recorded = journal.accept(entry)?;
    match recorded.events.first() {
        Some(RunEvent::AttemptRecorded {
            result: AttemptResult::Deferred { source },
            ..
        }) => Ok(AttemptCaptured::Deferred(source.clone())),
        Some(RunEvent::AttemptRecorded {
            result: AttemptResult::Done { output } | AttemptResult::Failed { output, .. },
            ..
        }) => Ok(AttemptCaptured::Captured(journal.materials.decode(output)?)),
        _ => Err(boundary(&call.call_id)),
    }
}

async fn capture_attempt(
    owner: MaterialOwner,
    call: &SingletonToolCall,
    member: &AdmittedCall,
    request: &SingletonPreparedRequest,
    handlers: &dyn SingletonToolHandlers,
    ordinal: AttemptOrdinal,
    completion_key: Option<&AwaitEventKey>,
) -> Result<crate::tool_run::RunAttemptEntry, String> {
    let declaration = &member.declaration;
    // The obligation material a declared start owns in this record.
    let mut started = Vec::new();
    let recorder = AttemptStreamRecorder::start();
    let outcome = if request.isolation.is_some() {
        None
    } else {
        Some(
            handlers
                .execute(SingletonAttempt {
                    call_id: &call.call_id,
                    attempt: ordinal,
                    request,
                    stream: &recorder,
                    completion_key,
                })
                .await?,
        )
    };
    let stream = recorder.finish();
    let capture = match outcome {
        None => Ok(SingletonCapture::Isolated {
            binding: Box::new(
                request
                    .isolation
                    .clone()
                    .ok_or("the isolated route has no admission")?,
            ),
        }),
        Some(SingletonBodyOutcome::Deferred { source }) => {
            match declaration.admits(OutcomeShape::Deferred) {
                Ok(()) if completion_key == Some(&source) => Err(source),
                Ok(()) => Ok(SingletonCapture::Refused {
                    refusal: DeclarationRefusal::UnarmedSource,
                }),
                Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
            }
        }
        Some(SingletonBodyOutcome::Done {
            commands,
            output,
            intents,
            start,
        }) => {
            // A declared start is a StartProcess intent of the result.
            let mut declared = intents.clone();
            if start.is_some() && !declared.contains(&ToolIntentKind::StartProcess) {
                declared.push(ToolIntentKind::StartProcess);
            }
            match declaration.admits(OutcomeShape::Done { intents: &declared }) {
                Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                Ok(()) => match start.map(|start| bind_start(call, &member.policy, *start)) {
                    None => Ok(SingletonCapture::Done {
                        output,
                        commands: commands.into_commands(),
                        intents,
                        stream,
                        start: None,
                    }),
                    Some(Err(refusal)) => Ok(SingletonCapture::StartRefused { refusal }),
                    Some(Ok(obligation)) => {
                        let (reference, entry) =
                            mint(&owner, MaterialRole::AttemptOutput, encode(&obligation)?)?;
                        started.push(entry);
                        Ok(SingletonCapture::Done {
                            output,
                            commands: commands.into_commands(),
                            intents,
                            stream,
                            start: Some(Box::new(SingletonStart {
                                start_key: obligation.start_key().clone(),
                                obligation: reference,
                            })),
                        })
                    }
                },
            }
        }
        Some(SingletonBodyOutcome::RetryableFailure { output, after_ms }) => {
            Ok(SingletonCapture::RetryableFailure {
                output,
                stream,
                after_ms,
            })
        }
        Some(SingletonBodyOutcome::Failed { output }) => {
            Ok(SingletonCapture::Failed { output, stream })
        }
    };
    let (result, materials) = match capture {
        Err(source) => (AttemptResult::Deferred { source }, Vec::new()),
        Ok(capture) => {
            let done = matches!(
                capture,
                SingletonCapture::Done { .. } | SingletonCapture::Isolated { .. }
            );
            let (output, entry) = mint(&owner, MaterialRole::AttemptOutput, encode(&capture)?)?;
            let result = if done {
                AttemptResult::Done { output }
            } else {
                AttemptResult::Failed {
                    output,
                    retryable: matches!(capture, SingletonCapture::RetryableFailure { .. }),
                }
            };
            started.insert(0, entry);
            (result, started)
        }
    };
    Ok(crate::tool_run::RunAttemptEntry {
        call_id: call.call_id.clone(),
        attempt: ordinal,
        result,
        materials,
    })
}

/// Why a physical boundary cannot admit work or capture the Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RunCutRefusal {
    #[error("the invocation failed; its engine journal owns recovery")]
    InvocationFailed,
    #[error("no physical cut was requested")]
    NotRequested,
    #[error("issued local work has not been durably acknowledged")]
    NotQuiescent,
    #[error("new admission is frozen for the {reason:?} cut")]
    AdmissionFrozen { reason: crate::BoundaryReason },
}

/// The acknowledged records a boundary may hand to continuation publication.
/// Canonical material and resolved state remain in their original receipts.
/// Pending Deferred sources remain descriptors in those records; no local
/// handle, body, socket or borrowed context is exported.
/// Turn and process owners retain their separate publication transactions.
#[derive(Clone, Debug)]
pub struct RunCutSnapshot {
    pub owner: EffectOpener,
    pub segment: SegmentOrdinal,
    pub cut: crate::tool_run::Cut,
    pub entries: Vec<RunJournalEntry>,
    pub attempts: Vec<crate::tool_run::RunAttemptEntry>,
}

impl RunCoordinator<'_> {
    fn begin_frame(&mut self) -> Result<(), SingletonRunError> {
        if self.active_frame || self.faulted {
            return Err(RunCutRefusal::InvocationFailed.into());
        }
        self.active_frame = true;
        Ok(())
    }

    fn note_fault<T>(&mut self, result: &Result<T, SingletonRunError>) {
        if result.is_err()
            && !matches!(
                result,
                Err(SingletonRunError::Cut(
                    RunCutRefusal::AdmissionFrozen { .. }
                ))
            )
        {
            self.faulted = true;
        }
    }

    /// Freeze admission at this boundary, retaining the first requested reason.
    /// Requesting a physical cut never closes or cancels the logical Run.
    pub fn request_cut(&mut self, reason: crate::BoundaryReason) -> crate::tool_run::Cut {
        let cut = *self
            .cut
            .get_or_insert_with(|| crate::tool_run::Cut::request(reason));
        let observed = cut.observe(
            self.pending
                .len()
                .saturating_add(usize::from(self.active_frame || self.faulted)),
        );
        self.cut = Some(observed);
        observed
    }

    /// The current phase, derived from the handles still owed durable acceptance.
    #[must_use]
    pub fn cut(&self) -> Option<crate::tool_run::Cut> {
        self.cut.map(|cut| {
            cut.observe(
                self.pending
                    .len()
                    .saturating_add(usize::from(self.active_frame || self.faulted)),
            )
        })
    }

    /// Poll issued work through durable acceptance, without draining protected
    /// declarations or awaiting Deferred sources. Registered retry work belongs
    /// to the already admitted calls and keeps its recorded schedule.
    ///
    /// # Errors
    /// A missing request or a typed execution refusal. An invocation fault
    /// exports nothing; its original engine journal owns recovery.
    pub async fn quiesce(&mut self) -> Result<RunCutSnapshot, SingletonRunError> {
        if self.cut.is_none() {
            return Err(RunCutRefusal::NotRequested.into());
        }
        while !self.pending.is_empty() {
            self.progress().await?;
        }
        self.capture_cut().map_err(Into::into)
    }

    /// Capture only durable receipts. This does not publish successor ownership.
    ///
    /// # Errors
    /// A missing request or an issued handle still awaiting durable acceptance.
    pub fn capture_cut(&self) -> Result<RunCutSnapshot, RunCutRefusal> {
        if self.faulted || self.active_frame {
            return Err(RunCutRefusal::InvocationFailed);
        }
        let cut = self.cut().ok_or(RunCutRefusal::NotRequested)?;
        if cut.phase != crate::tool_run::CutPhase::Capturable {
            return Err(RunCutRefusal::NotQuiescent);
        }
        Ok(RunCutSnapshot {
            owner: self.journal.owner.clone(),
            segment: self.journal.segment,
            cut,
            entries: self.journal.entries.clone(),
            attempts: self.attempts.clone(),
        })
    }
}
