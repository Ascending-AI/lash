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

mod aggregate;
mod continuation;
mod deferred;
mod drain;
mod parallel;
mod start;
use start::{bind_start, discharge_start, drain_start, launch_start, recorded_obligation};

pub use aggregate::RunAggregateOutcome;

use lash_sansio::ToolCallId;

use lash_sansio::ToolIntentKind;

use super::singleton_run::{
    BeforeCheckReply, RecordedIsolatedStart, RecordedPreparedRequest, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonDrift, SingletonPreparedRequest,
    SingletonRunError, SingletonStart, SingletonTerminal, SingletonToolCall, SingletonToolHandlers,
};
use crate::runtime::effect::{AttemptStreamRecorder, ScopedEffectController};
use crate::runtime::process::{
    DeclaredStartObligation, DeclaredStartObligationRefusal, IsolatedStartRefusal,
    IsolatedToolStart, ProcessExecutionBoundary,
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
    snapshots: BTreeMap<MaterialRef, std::sync::Arc<crate::plugin::PluginNamespaceState>>,
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

    fn snapshot(
        &mut self,
        reference: &MaterialRef,
    ) -> Result<std::sync::Arc<crate::plugin::PluginNamespaceState>, RuntimeEffectControllerError>
    {
        if reference.role != MaterialRole::PluginStateSnapshot {
            return Err(MaterialRefusal::RoleMismatch {
                reference: Box::new(reference.clone()),
                expected: MaterialRole::PluginStateSnapshot,
                found: reference.role,
            }
            .into());
        }
        self.read(reference)?;
        if let Some(snapshot) = self.snapshots.get(reference) {
            return Ok(std::sync::Arc::clone(snapshot));
        }
        let snapshot = std::sync::Arc::new(self.decode(reference)?);
        self.snapshots
            .insert(reference.clone(), std::sync::Arc::clone(&snapshot));
        Ok(snapshot)
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
/// admission record is written, inside its step. Served replay never validates
/// the live declaration or selects a replacement route.
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
    snapshot: Option<(
        MaterialRef,
        std::sync::Arc<crate::plugin::PluginNamespaceState>,
    )>,
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
        environment: call.environment.clone(),
        prepared: handlers.prepare(call).await?,
        state_snapshot: snapshot
            .as_ref()
            .map(|(_, namespace)| std::sync::Arc::clone(namespace)),
        isolation,
    };
    let (request_ref, request_entry) = mint(
        owner,
        MaterialRole::PreparedRequest,
        encode(&RecordedPreparedRequest {
            arguments: request.arguments.clone(),
            environment: request.environment.clone(),
            prepared: request.prepared.clone(),
            state_snapshot: snapshot.map(|(reference, _)| reference),
            isolation: request.isolation.clone(),
        })?,
    )?;
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
    journal: &mut RunJournal<'_>,
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
    let recorded: RecordedPreparedRequest = journal.materials.decode(&member.request)?;
    let request = SingletonPreparedRequest {
        arguments: recorded.arguments,
        environment: recorded.environment,
        prepared: recorded.prepared,
        state_snapshot: recorded
            .state_snapshot
            .as_ref()
            .map(|reference| journal.materials.snapshot(reference))
            .transpose()?,
        isolation: recorded.isolation,
    };
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
    let segment = plugins.state_segment();
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
            .reduce_proposals(&address, segment, proposals)
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

#[derive(Clone)]
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
    start: Option<SingletonStart>,
}

struct PresentedCall {
    decision: CallDecision,
    presentation: Option<MaterialRef>,
    launched: Option<ProcessId>,
}

/// The calls of one logical Run, recorded in its opener journal.
pub struct RunCoordinator<'a> {
    journal: RunJournal<'a>,
    /// Decided calls whose presentation is owed, by rank.
    owed: BTreeMap<u64, Owed<'a>>,
    pending: Vec<parallel::Pending<'a>>,
    attempts: Vec<crate::tool_run::RunAttemptEntry>,
    handlers: BTreeMap<ToolCallId, Handlers<'a>>,
    presented: BTreeMap<ToolCallId, PresentedCall>,
    timers: Vec<parallel::AggregateTimer<'a>>,
    cut: Option<crate::tool_run::Cut>,
    faulted: bool,
    active_frame: bool,
    sources: BTreeMap<ToolCallId, crate::tool_run::SourceDescriptor>,
    waiting: BTreeMap<ToolCallId, Waiting<'a>>,
    process_sources: BTreeMap<ToolCallId, AwaitEventKey>,
    pending_starts: BTreeMap<ToolCallId, (Waiting<'a>, AwaitEventKey)>,
    environment: Option<crate::ProcessExecutionEnvRef>,
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
                    snapshots: BTreeMap::new(),
                },
                records: Vec::new(),
                entries: Vec::new(),
            },
            owed: BTreeMap::new(),
            pending: Vec::new(),
            attempts: Vec::new(),
            handlers: BTreeMap::new(),
            presented: BTreeMap::new(),
            timers: Vec::new(),
            cut: None,
            faulted: false,
            active_frame: false,
            sources: BTreeMap::new(),
            waiting: BTreeMap::new(),
            process_sources: BTreeMap::new(),
            pending_starts: BTreeMap::new(),
            environment: None,
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
        self.handlers
            .insert(call.call_id.clone(), Handlers::Borrowed(handlers));
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
                    self.process_sources.get(&call.call_id),
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
                    AttemptCaptured::DeferredStart { source, start } => {
                        return Ok(self.queue_deferred_start(
                            call,
                            member,
                            Handlers::Borrowed(handlers),
                            AttemptOrdinal::FIRST,
                            source,
                            *start,
                        ));
                    }
                    AttemptCaptured::Deferred(source) => {
                        self.waiting.insert(
                            call.call_id.clone(),
                            Waiting {
                                call: call.clone(),
                                member,
                                handlers: Handlers::Borrowed(handlers),
                                attempt: AttemptOrdinal::FIRST,
                                start: None,
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
        self.admit_round(std::slice::from_ref(call), handlers, retry, None)
            .await?
            .pop()
            .ok_or_else(|| boundary(&call.call_id))
    }

    async fn admit_round(
        &mut self,
        calls: &[SingletonToolCall],
        handlers: &dyn SingletonToolHandlers,
        retry: crate::tool_run::RecordedRetryPolicy,
        aggregate: Option<(&crate::tool_run::AggregatePlan, &dyn crate::Clock)>,
    ) -> Result<Vec<(AdmittedCall, SingletonPreparedRequest)>, SingletonRunError> {
        if self.faulted {
            return Err(RunCutRefusal::InvocationFailed.into());
        }
        if let Some(cut) = self.cut {
            return Err(RunCutRefusal::AdmissionFrozen { reason: cut.reason }.into());
        }
        if self.journal.ledger.lifecycle() != crate::tool_run::RunLifecycle::Live
            || self.journal.ledger.aborted()
        {
            return Err(RunEventRefusal::AdmissionClosed.into());
        }
        if calls.is_empty() && aggregate.is_none() {
            return Ok(Vec::new());
        }
        let mut ids = std::collections::BTreeSet::new();
        for call in calls {
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
        }
        let name = aggregate.map_or_else(
            || record_name(&calls[0].call_id, "admit"),
            |(plan, _)| format!("lash:run:aggregate:{}:admit", plan.key),
        );
        let journal = &mut self.journal;
        let first = journal.record(Vec::new());
        let owner = journal.materials.owner.clone();
        let journal_owner = journal.owner.clone();
        let known_material = journal
            .materials
            .entries
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut live_refusal = None;
        let refusal = &mut live_refusal;
        let admit = Box::pin(async move {
            let starts = calls
                .iter()
                .enumerate()
                .map(|(index, call)| admit_live(call, handlers, index))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    let message = error.to_string();
                    *refusal = Some(error);
                    message
                })?;
            let mut members = Vec::with_capacity(calls.len());
            let mut materials = Vec::new();
            let state = handlers
                .plugin_session()
                .map(|plugins| plugins.export_state());
            let mut snapshots = BTreeMap::new();
            for (call, start) in calls.iter().zip(starts) {
                let revision = &call.binding.executable.owner;
                let snapshot = if let Some(state) = &state {
                    let key = (revision.plugin.clone(), revision.behavior_revision.get());
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        snapshots.entry(key.clone())
                    {
                        let namespace = std::sync::Arc::new(
                            state
                                .plugins
                                .get(&revision.plugin)
                                .cloned()
                                .unwrap_or_default(),
                        );
                        let payload = MaterialPayload::new(
                            owner.clone(),
                            MaterialRole::PluginStateSnapshot,
                            Some(revision.clone()),
                            encode(namespace.as_ref())?,
                        );
                        let reference = payload
                            .reference(MaterialLocation::JournalLocal)
                            .map_err(|error| error.to_string())?;
                        materials.push(MaterialEntry::Available {
                            reference: reference.clone(),
                            payload: Box::new(payload),
                        });
                        entry.insert((reference, namespace));
                    }
                    snapshots.get(&key).cloned()
                } else {
                    None
                };
                let (member, minted) =
                    prepare_admitted_call(&owner, call, handlers, start, snapshot, retry.clone())
                        .await?;
                members.push(member);
                materials.extend(minted);
            }
            materials.retain(|entry| match entry {
                MaterialEntry::Available { reference, .. }
                | MaterialEntry::Retired { reference } => !known_material.contains(reference),
            });
            let operands = (0..members.len())
                .map(u32::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            let mut events = vec![RunEvent::Admitted {
                round: RoundAdmission {
                    owner: journal_owner,
                    members,
                    operands,
                },
            }];
            if let Some((plan, clock)) = aggregate {
                events.push(RunEvent::AggregateAdmitted {
                    plan: plan.clone(),
                    admitted_at_ms: clock.timestamp_ms(),
                });
            }
            Ok(RunJournalEntry {
                record: RunRecord { events, ..first },
                materials,
                state: Vec::new(),
            })
        });
        let admitted = match journal.append(name, admit).await {
            Ok(record) => record,
            Err(error) => return Err(live_refusal.unwrap_or(error)),
        };
        let Some(RunEvent::Admitted { round }) = admitted.events.first() else {
            return Err(RunEventRefusal::AggregateShape {
                key: aggregate.map_or_else(String::new, |(plan, _)| plan.key.clone()),
            }
            .into());
        };
        if round.members.len() != calls.len()
            || round.operands
                != (0..calls.len())
                    .map(u32::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| RunEventRefusal::AggregateShape {
                        key: aggregate.map_or_else(String::new, |(plan, _)| plan.key.clone()),
                    })?
        {
            return Err(RunEventRefusal::AggregateShape {
                key: aggregate.map_or_else(String::new, |(plan, _)| plan.key.clone()),
            }
            .into());
        }
        if let Some((plan, _)) = aggregate
            && !admitted.events.iter().any(|event| matches!(event, RunEvent::AggregateAdmitted { plan: recorded, .. } if recorded == plan))
        {
            return Err(RunEventRefusal::AggregateShape { key: plan.key.clone() }.into());
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
        for (call, (member, request)) in calls.iter().zip(&admitted) {
            if self.environment.is_none() {
                self.environment.clone_from(&request.environment);
            }
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
                let process_source = journal
                    .scoped
                    .controller()
                    .await_event_key(
                        call.owner.admitted_scope().scope(),
                        crate::AwaitEventWaitIdentity::Custom {
                            key: format!("declared-process-terminal:{}", call.call_id),
                        },
                    )
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                self.process_sources
                    .insert(call.call_id.clone(), process_source);
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
                aborted: journal.ledger.aborted()
                    || journal.ledger.lifecycle() != crate::tool_run::RunLifecycle::Live,
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
}

fn boundary(call_id: &ToolCallId) -> SingletonRunError {
    RunEventRefusal::BoundaryOrder {
        call_id: call_id.clone(),
    }
    .into()
}

enum AttemptCaptured {
    Captured(SingletonCapture),
    Deferred(AwaitEventKey),
    DeferredStart {
        source: AwaitEventKey,
        start: Box<SingletonStart>,
    },
}

struct AttemptSources<'a> {
    completion: Option<&'a AwaitEventKey>,
    process: Option<&'a AwaitEventKey>,
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
    process_source: Option<&AwaitEventKey>,
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
            AttemptSources {
                completion: completion_key,
                process: process_source,
            },
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
            result:
                AttemptResult::DeferredStart {
                    source,
                    start_key,
                    obligation,
                },
            ..
        }) => Ok(AttemptCaptured::DeferredStart {
            source: source.clone(),
            start: Box::new(SingletonStart {
                start_key: start_key.clone(),
                obligation: obligation.clone(),
            }),
        }),
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
    sources: AttemptSources<'_>,
) -> Result<crate::tool_run::RunAttemptEntry, String> {
    let AttemptSources {
        completion: completion_key,
        process: process_source,
    } = sources;
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
        Some(SingletonBodyOutcome::DeferredStart { start }) => {
            let admitted = declaration.admits(OutcomeShape::Deferred).and_then(|()| {
                declaration.admits(OutcomeShape::Done {
                    intents: &[ToolIntentKind::StartProcess],
                })
            });
            match admitted {
                Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                Ok(()) => match bind_start(call, &member.policy, *start) {
                    Err(refusal) => Ok(SingletonCapture::StartRefused { refusal }),
                    Ok(obligation) => {
                        let source = process_source
                            .ok_or("the process-terminal source was not reserved")?
                            .clone();
                        let (reference, entry) =
                            mint(&owner, MaterialRole::AttemptOutput, encode(&obligation)?)?;
                        return Ok(crate::tool_run::RunAttemptEntry {
                            call_id: call.call_id.clone(),
                            attempt: ordinal,
                            result: AttemptResult::DeferredStart {
                                source,
                                start_key: obligation.start_key().clone(),
                                obligation: reference,
                            },
                            materials: vec![entry],
                        });
                    }
                },
            }
        }
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
}
