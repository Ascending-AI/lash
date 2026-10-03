//! The Run coordinator (K3, FIG-4877, FIG-4880): the calls of one logical Run
//! record their admission (A), attempt (X), decision (D), declarations and
//! presentation with incorporation (V) in the Run's opener journal, and the
//! Run drains every committed final's protected work in rank order.
//!
//! Each call is admitted as a singleton round. Its decision takes the Run's
//! next rank, so ranks follow the order decisions became durable. A final
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
//! recorded schedule are FIG-4879's. While a final's declarations drain, any
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
        reference.verify(&self.owner, &reference.digest)?;
        match self.entries.get(reference) {
            Some(Some(payload)) => Ok(&payload.text),
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
    segment: SegmentOrdinal,
    ledger: RunLedger,
    materials: Materials,
    records: Vec<RunRecord>,
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
        self.ledger.append(self.segment, &entry.record)?;
        let references = entry
            .materials
            .iter()
            .map(|material| match material {
                MaterialEntry::Available { reference, .. }
                | MaterialEntry::Retired { reference } => reference.clone(),
            })
            .collect();
        self.materials.admit(entry.materials)?;
        self.records.push(entry.record.clone());
        Ok((entry.record, references))
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
) -> Result<Option<IsolatedToolStart>, SingletonRunError> {
    call.declaration
        .validate()
        .map_err(|cause| AdmissionRefusal::Declaration { member: 0, cause })?;

    call.binding
        .require_available(&call.available)
        .map_err(|cause| AdmissionRefusal::BindingUnavailable {
            member: 0,
            cause: Box::new(cause),
        })?;
    if !call.declaration.isolated {
        return Ok(None);
    }
    let start = handlers
        .isolated_start(call)
        .ok_or(AdmissionRefusal::UnsupportedIsolation { member: 0 })?;
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

/// A decided call whose presentation the drain owes.
struct Owed<'a> {
    call_id: ToolCallId,
    handlers: &'a dyn SingletonToolHandlers,
    decision: CallDecision,
    /// The result candidate the decision checked, when one existed.
    capture: Option<SingletonCapture>,
}

/// The calls of one logical Run, recorded in its opener journal.
pub struct RunCoordinator<'a> {
    journal: RunJournal<'a>,
    /// Decided calls whose presentation is owed, by rank.
    owed: BTreeMap<u64, Owed<'a>>,
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
                segment,
                ledger: RunLedger::new(owner.clone()),
                materials: Materials {
                    owner: MaterialOwner::Run { opener: owner },
                    available,
                    entries: BTreeMap::new(),
                },
                records: Vec::new(),
            },
            owed: BTreeMap::new(),
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
        let live_start = admit_live(call, handlers)?;
        let journal = &mut self.journal;

        // A: preparation and every before-check, on one prepared request.
        let first = journal.record(Vec::new());
        let owner = journal.materials.owner.clone();
        let admit = Box::pin(async move {
            let mut minted = Vec::new();
            let isolation = match live_start {
                None => None,
                Some(start) => {
                    let Some(crate::ProcessInput::Engine { kind, .. }) =
                        start.registration.input.input()
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
                        mint(&owner, MaterialRole::PreparedRequest, encode(&obligation)?)?;
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
                isolation,
            };
            let (request_ref, request_entry) =
                mint(&owner, MaterialRole::PreparedRequest, encode(&request)?)?;
            minted.push(request_entry);
            let mut checks = Vec::new();
            for reply in handlers.before_checks(call, &request).await {
                checks.push(before_verdict(&owner, reply, &mut minted)?);
            }
            let round = RoundAdmission {
                owner: call.owner.clone(),
                members: vec![AdmittedCall {
                    call_id: call.call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    request: request_ref,
                    declaration: call.declaration.clone(),
                    binding: call.binding.clone(),
                    policy: RuntimeCallPolicy {
                        cancel: call.cancel,
                        ..RuntimeCallPolicy::default()
                    },
                    checks: CheckRecord::reduce(checks),
                }],
                operands: vec![0],
            };
            Ok(RunJournalEntry {
                record: RunRecord {
                    events: vec![RunEvent::Admitted { round }],
                    ..first
                },
                materials: minted,
            })
        });
        let admitted = journal
            .append(record_name(&call.call_id, "admit"), admit)
            .await?;
        let Some(RunEvent::Admitted { round }) = admitted.events.first() else {
            return Err(boundary(&call.call_id));
        };
        let member = match round.members.as_slice() {
            [member] if member.call_id == call.call_id => member.clone(),
            _ => {
                return Err(SingletonRunError::Drift {
                    call_id: call.call_id.clone(),
                    drift: SingletonDrift::CallId,
                });
            }
        };
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
        round.clone().admit(&journal.materials.available, |_| {
            request.isolation.is_some()
        })?;
        if member.declaration.isolated {
            let binding = request
                .isolation
                .as_ref()
                .ok_or(AdmissionRefusal::UnsupportedIsolation { member: 0 })?;
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

        // The result candidate the decision checks, and where it came from.
        let candidate = match member.selection() {
            BeforeSelection::Execute => {
                match attempt(journal, call, &member, &request, handlers).await? {
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

        // D: the one final-or-cancel decision, under the Run's next rank. The
        // Run's cancellation is read here and nowhere else, so the decision
        // chooses once.
        let selection = member.selection();
        let rank = journal.ledger.next_rank();
        let decide_record = journal.record(Vec::new());
        let checked = candidate.clone();
        let decide = Box::pin(async move {
            let (decision, after) = match (selection, checked) {
                (BeforeSelection::Deny, _) => (CallDecision::Denied, None),
                (BeforeSelection::Cancel, _) => (CallDecision::Cancelled, None),
                (BeforeSelection::AbortRun, _) => (CallDecision::Aborted, None),
                (_, None) => return Err("a result candidate has no capture".to_owned()),
                (_, Some(_)) if handlers.run_cancel_requested() => (CallDecision::Cancelled, None),
                (_, Some((source, capture))) => {
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
            Ok(RunJournalEntry {
                record: RunRecord {
                    events: vec![RunEvent::Decided {
                        call_id: call.call_id.clone(),
                        rank,
                        decision,
                        after,
                    }],
                    ..decide_record
                },
                materials: Vec::new(),
            })
        });
        let decided = journal
            .append(record_name(&call.call_id, "decide"), decide)
            .await?;
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
) -> Result<AttemptCaptured, SingletonRunError> {
    let record = journal.record(Vec::new());
    let owner = journal.materials.owner.clone();
    let declaration = &member.declaration;
    let step = Box::pin(async move {
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
                        attempt: AttemptOrdinal::FIRST,
                        request,
                        stream: &recorder,
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
                    Ok(()) => Err(source),
                    Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                }
            }
            Some(SingletonBodyOutcome::Done {
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
                        retryable: false,
                    }
                };
                started.insert(0, entry);
                (result, started)
            }
        };
        Ok(RunJournalEntry {
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
    let recorded = journal
        .append(record_name(&call.call_id, "attempt:1"), step)
        .await?;
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
