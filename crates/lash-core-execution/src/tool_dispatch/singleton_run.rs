//! The singleton Run route (K3, FIG-4877): one admitted tool call records its
//! admission (A), its attempt (X), its decision (D) and its presentation with
//! its incorporation (V) as four records in its owning Run's opener journal.
//!
//! This is an expansion interface, not a permanent execution mode. General
//! production callers keep their existing route until the Run coordinator
//! carries them (F01-F03). The route opens no child invocation, makes no
//! group call and acquires no per-call environment: every record is one
//! journaled step of the caller's own handler, through
//! [`RuntimeEffectController::record_run_record`].
//!
//! Each record's step runs once. A replay serves the journaled record without
//! running its step, so a recorded attempt never re-executes its body, a
//! recorded check never runs again, and a recorded decision never observes
//! cancellation again. A step that did not become durable — its fault, or a
//! crash before the engine acknowledged its result — runs again with the same
//! call id and attempt ordinal. Every served record goes through the
//! [`RunLedger`] fold before anything acts on it, and every material reference
//! it names is verified against its canonical bytes.
//!
//! The order is the K3 contract: admission before any body; one durable
//! attempt before the decision; one final-or-cancel decision; a final's
//! declarations issued and settled before its presentation; presentation in
//! the same record as its incorporation. The Run's cancellation is read only
//! inside the decision's step, so the decision chooses once: a cancellation
//! before the decision is durable cancels the call, and one after it cannot
//! abandon the final's protected declarations or presentation. An issued
//! attempt always settles through its record before the decision.
//!
//! Reported retries (K9) are the Run coordinator's schedule (FIG-4879); a
//! singleton admits no retry policy. A Deferred attempt hands its call to the
//! source seal (FIG-4883) after its record.
//!
//! [`RuntimeEffectController::record_run_record`]: crate::RuntimeEffectController::record_run_record

use std::collections::BTreeMap;

use lash_sansio::{ToolCallId, ToolIntentKind};
use serde::{Deserialize, Serialize};

use crate::runtime::effect::ScopedEffectController;
use crate::store::plugin_writers::PluginRevision;
use crate::tool_run::{
    AdmissionRefusal, AdmittedBinding, AdmittedCall, AfterCheckVerdict, AttemptOrdinal,
    AttemptResult, AttributedVerdict, BeforeCheckVerdict, BeforeSelection, CallDecision,
    CheckRecord, DeclarationRefusal, HookCause, MaterialEntry, MaterialLocation, MaterialOwner,
    MaterialPayload, MaterialRef, MaterialRefusal, MaterialRole, OutcomeShape, ResultSource,
    RoundAdmission, RunEvent, RunEventRefusal, RunJournalEntry, RunLedger, RunRecord,
    RuntimeCallPolicy, SegmentOrdinal, ToolDeclaration,
};
use crate::{AwaitEventKey, EffectOpener, RuntimeEffectControllerError};

/// The rank of a singleton's one decision.
const SINGLETON_RANK: u64 = 0;

/// One tool call to run as a singleton in its owning Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingletonToolCall {
    /// The opener of the logical Run that owns the call.
    pub owner: EffectOpener,
    /// The active segment of that Run, which appends every record.
    pub segment: SegmentOrdinal,
    pub call_id: ToolCallId,
    pub tool_name: String,
    /// The request as the model issued it.
    pub arguments: serde_json::Value,
    /// The declaration of the manifest the call is admitted under. A recorded
    /// admission's declaration governs every replay; this one is admitted only
    /// on the first execution.
    pub declaration: ToolDeclaration,
    /// The executable, preparation and presentation callbacks the admission
    /// binds, each with its plugin revision.
    pub binding: AdmittedBinding,
    /// The plugin revisions this build executes. A recorded admission bound to
    /// any other refuses, typed, before its body.
    pub available: Vec<PluginRevision>,
}

/// The prepared request admission records (A): the request as issued and the
/// payload preparation made of it, after argument transforms and before any
/// check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingletonPreparedRequest {
    pub arguments: serde_json::Value,
    pub prepared: serde_json::Value,
}

/// What an attempt captured (X), or the cached success a before-check
/// supplied in its place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SingletonCapture {
    Done {
        output: String,
        /// The declared Lash intents the result asks the Run to realize.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        intents: Vec<ToolIntentKind>,
    },
    /// A failure the body reported.
    Failed { output: String },
    /// An outcome the admitted declaration does not admit, refused before
    /// anything it declared was realized.
    Refused { refusal: DeclarationRefusal },
}

impl SingletonCapture {
    /// The result text, when the capture has one.
    #[must_use]
    pub fn output(&self) -> Option<&str> {
        match self {
            Self::Done { output, .. } | Self::Failed { output } => Some(output),
            Self::Refused { .. } => None,
        }
    }

    fn intents(&self) -> &[ToolIntentKind] {
        match self {
            Self::Done { intents, .. } => intents,
            Self::Failed { .. } | Self::Refused { .. } => &[],
        }
    }
}

/// What one attempt's body returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SingletonBodyOutcome {
    Done {
        output: String,
        intents: Vec<ToolIntentKind>,
    },
    Failed {
        output: String,
    },
    /// Parked on a Deferred source; the source's seal supplies the result.
    Deferred {
        source: AwaitEventKey,
    },
}

/// The attempt a body executes: its call, its ordinal and its admitted
/// request. A crash redelivery presents the same call id and ordinal.
#[derive(Clone, Copy, Debug)]
pub struct SingletonAttempt<'a> {
    pub call_id: &'a ToolCallId,
    pub attempt: AttemptOrdinal,
    pub request: &'a SingletonPreparedRequest,
}

/// A before-check's reply, before admission records it. A cached success is
/// the result text only; admission records it as the Run's attempt output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeforeCheckReply {
    Allow,
    Cached { output: String },
    Deny { cause: HookCause },
    Cancel { cause: HookCause },
    AbortRun { cause: HookCause },
}

/// The callbacks a singleton's records run. Each runs inside the step of the
/// record that owns its answer and never on a replay that serves the record.
/// An `Err` is a fault: the record stays unjournaled and its step runs again.
#[async_trait::async_trait]
pub trait SingletonToolHandlers: Send + Sync {
    /// Prepare the request (A).
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String>;

    /// Every before-check's reply on the one prepared request (A).
    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Vec<AttributedVerdict<BeforeCheckReply>>;

    /// Execute the body once (X).
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String>;

    /// Every after-check's reply on the result candidate (D).
    async fn after_checks(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>>;

    /// Whether the owning Run's cancellation is requested, read once inside
    /// the decision's step (D).
    fn run_cancel_requested(&self) -> bool;

    /// Realize a final's declared intents behind their exactly-once fences.
    /// A crash before the declarations settle realizes them again, so the
    /// fence is what makes them once.
    async fn realize_declarations(
        &self,
        call_id: &ToolCallId,
        intents: &[ToolIntentKind],
    ) -> Result<(), String>;

    /// The model-facing presentation of a final result (V).
    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, String>;
}

/// How a singleton ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SingletonTerminal {
    /// The result is final, its declarations settled, and it is presented and
    /// incorporated.
    Final {
        source: ResultSource,
        capture: SingletonCapture,
        presentation: String,
    },
    /// A check or the Run's cancellation withheld the result: the decision is
    /// denied, cancelled or aborted, and the recorded check names the cause.
    Withheld { decision: CallDecision },
    /// The attempt parked on a Deferred source; the source's seal finishes
    /// the call.
    Deferred { source: AwaitEventKey },
}

/// A finished singleton: how it ended and the records its Run holds.
#[derive(Clone, Debug)]
pub struct SingletonRunOutcome {
    pub terminal: SingletonTerminal,
    pub records: Vec<RunRecord>,
}

/// What a recorded admission names differently from the call replaying it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SingletonDrift {
    CallId,
    ToolName,
    Arguments,
}

/// Why a singleton stopped before it ended. None of these runs a body.
#[derive(Debug, thiserror::Error)]
pub enum SingletonRunError {
    /// Admission refused the call, or a recorded admission no longer binds an
    /// available plugin revision.
    #[error("admission refused call: {0}")]
    Admission(#[from] AdmissionRefusal),
    /// The recorded admission belongs to another request under this call id.
    #[error("the recorded admission of call {call_id} names another {drift:?}")]
    Drift {
        call_id: ToolCallId,
        drift: SingletonDrift,
    },
    /// A served record breaks the Run's event contract.
    #[error("a Run record was refused: {0}")]
    Ledger(#[from] RunEventRefusal),
    /// The engine, or a material read, refused; a material refusal carries
    /// its typed cause.
    #[error(transparent)]
    Controller(#[from] RuntimeEffectControllerError),
}

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

fn encode<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

/// The fold and the material of one singleton's Run as its records are
/// served.
struct SingletonJournal<'a> {
    scoped: &'a ScopedEffectController<'a>,
    segment: SegmentOrdinal,
    ledger: RunLedger,
    materials: Materials,
    records: Vec<RunRecord>,
}

impl SingletonJournal<'_> {
    fn record(&self, events: Vec<RunEvent>) -> RunRecord {
        RunRecord {
            segment: self.segment,
            first: self.ledger.next_ordinal(),
            events,
        }
    }

    /// Journal one record, or serve the recorded one, and admit it through
    /// the fold and the material check before anything acts on it.
    async fn append(
        &mut self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<RunRecord, SingletonRunError> {
        self.scoped.admit_journal_write()?;
        let entry = self
            .scoped
            .controller()
            .record_run_record(name, step)
            .await?;
        self.ledger.append(self.segment, &entry.record)?;
        self.materials.admit(entry.materials)?;
        self.records.push(entry.record.clone());
        Ok(entry.record)
    }
}

fn record_name(call_id: &ToolCallId, step: &str) -> String {
    format!("lash:run:{call_id}:{step}")
}

/// Admission's typed refusal of the call itself, checked before the
/// admission record is written so that no refused call is ever recorded.
fn admit_live(call: &SingletonToolCall) -> Result<(), AdmissionRefusal> {
    call.declaration
        .validate()
        .map_err(|cause| AdmissionRefusal::Declaration { member: 0, cause })?;
    if call.declaration.isolated {
        return Err(AdmissionRefusal::UnsupportedIsolation { member: 0 });
    }
    call.binding
        .require_available(&call.available)
        .map_err(|cause| AdmissionRefusal::BindingUnavailable {
            member: 0,
            cause: Box::new(cause),
        })
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

/// Run `call` as a singleton in its owning Run, recording A, X, D and V in
/// the opener journal `scoped` serves.
///
/// # Errors
///
/// A typed [`SingletonRunError`]; none of them executes a body.
pub async fn run_singleton_tool(
    scoped: &ScopedEffectController<'_>,
    call: &SingletonToolCall,
    handlers: &dyn SingletonToolHandlers,
) -> Result<SingletonRunOutcome, SingletonRunError> {
    admit_live(call)?;
    let mut journal = SingletonJournal {
        scoped,
        segment: call.segment,
        ledger: RunLedger::new(call.owner.clone()),
        materials: Materials {
            owner: MaterialOwner::Run {
                opener: call.owner.clone(),
            },
            available: call.available.clone(),
            entries: BTreeMap::new(),
        },
        records: Vec::new(),
    };

    // A: preparation and every before-check, on one prepared request.
    let first = journal.record(Vec::new());
    let owner = journal.materials.owner.clone();
    let admit = Box::pin(async move {
        let request = SingletonPreparedRequest {
            arguments: call.arguments.clone(),
            prepared: handlers.prepare(call).await?,
        };
        let (request_ref, request_entry) =
            mint(&owner, MaterialRole::PreparedRequest, encode(&request)?)?;
        let mut minted = vec![request_entry];
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
                policy: RuntimeCallPolicy::default(),
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
        return Err(RunEventRefusal::BoundaryOrder {
            call_id: call.call_id.clone(),
        }
        .into());
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
    round.clone().admit(&call.available, |_| false)?;

    // The result candidate the decision checks, and where it came from.
    let candidate = match member.selection() {
        BeforeSelection::Execute => {
            match attempt(&mut journal, call, &member, &request, handlers).await? {
                AttemptCaptured::Captured(capture) => Some((
                    ResultSource::Attempt {
                        attempt: AttemptOrdinal::FIRST,
                    },
                    capture,
                )),
                AttemptCaptured::Deferred(source) => {
                    return Ok(SingletonRunOutcome {
                        terminal: SingletonTerminal::Deferred { source },
                        records: journal.records,
                    });
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
            Some((ResultSource::Cached, journal.materials.decode(result)?))
        }
        BeforeSelection::Deny | BeforeSelection::Cancel | BeforeSelection::AbortRun => None,
    };

    // D: the one final-or-cancel decision. The Run's cancellation is read
    // here and nowhere else, so the decision chooses once.
    let selection = member.selection();
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
                        declares: !capture.intents().is_empty(),
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
                    rank: SINGLETON_RANK,
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
    let Some(RunEvent::Decided { decision, .. }) = decided.events.first() else {
        return Err(RunEventRefusal::BoundaryOrder {
            call_id: call.call_id.clone(),
        }
        .into());
    };
    let decision = decision.clone();

    let (CallDecision::Final { declares, source }, Some((_, capture))) = (&decision, candidate)
    else {
        // V: a withheld call is presented by its decision and incorporated.
        let present = journal.record(presented(&call.call_id, None));
        journal
            .append(
                record_name(&call.call_id, "present"),
                Box::pin(async move {
                    Ok(RunJournalEntry {
                        record: present,
                        materials: Vec::new(),
                    })
                }),
            )
            .await?;
        return Ok(SingletonRunOutcome {
            terminal: SingletonTerminal::Withheld { decision },
            records: journal.records,
        });
    };

    // A final's declarations are issued only after its decision is durable,
    // and settle before its presentation.
    let mut settle = Vec::new();
    if *declares {
        let issued = journal.record(vec![RunEvent::DeclarationsIssued {
            call_id: call.call_id.clone(),
        }]);
        journal
            .append(
                record_name(&call.call_id, "declare"),
                Box::pin(async move {
                    Ok(RunJournalEntry {
                        record: issued,
                        materials: Vec::new(),
                    })
                }),
            )
            .await?;
        settle.push(RunEvent::DeclarationsSettled {
            call_id: call.call_id.clone(),
        });
    }

    // V: presentation, owning only bytes distinct from the output, in one
    // record with its incorporation.
    let present_record = journal.record(Vec::new());
    let owner = journal.materials.owner.clone();
    let final_capture = capture.clone();
    let declares = *declares;
    let present = Box::pin(async move {
        if declares {
            handlers
                .realize_declarations(&call.call_id, final_capture.intents())
                .await?;
        }
        let text = handlers.present(&call.call_id, &final_capture).await?;
        let mut owned = Vec::new();
        let presentation = if final_capture.output() == Some(text.as_str()) {
            None
        } else {
            let (reference, entry) = mint(&owner, MaterialRole::Presentation, text)?;
            owned.push(entry);
            Some(reference)
        };
        let mut events = settle;
        events.extend(presented(&call.call_id, presentation));
        Ok(RunJournalEntry {
            record: RunRecord {
                events,
                ..present_record
            },
            materials: owned,
        })
    });
    let presented_record = journal
        .append(record_name(&call.call_id, "present"), present)
        .await?;
    let presentation = presented_record
        .events
        .iter()
        .find_map(|event| match event {
            RunEvent::Presented { presentation, .. } => Some(presentation.clone()),
            _ => None,
        });
    let presentation =
        match presentation.flatten() {
            Some(reference) => journal.materials.read(&reference)?.to_owned(),
            None => capture.output().map(str::to_owned).ok_or_else(|| {
                RunEventRefusal::BoundaryOrder {
                    call_id: call.call_id.clone(),
                }
            })?,
        };
    Ok(SingletonRunOutcome {
        terminal: SingletonTerminal::Final {
            source: source.clone(),
            capture,
            presentation,
        },
        records: journal.records,
    })
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
/// before its record admits anything it declared.
async fn attempt(
    journal: &mut SingletonJournal<'_>,
    call: &SingletonToolCall,
    member: &AdmittedCall,
    request: &SingletonPreparedRequest,
    handlers: &dyn SingletonToolHandlers,
) -> Result<AttemptCaptured, SingletonRunError> {
    let record = journal.record(Vec::new());
    let owner = journal.materials.owner.clone();
    let declaration = &member.declaration;
    let step = Box::pin(async move {
        let outcome = handlers
            .execute(SingletonAttempt {
                call_id: &call.call_id,
                attempt: AttemptOrdinal::FIRST,
                request,
            })
            .await?;
        let capture = match outcome {
            SingletonBodyOutcome::Deferred { source } => {
                match declaration.admits(OutcomeShape::Deferred) {
                    Ok(()) => Err(source),
                    Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                }
            }
            SingletonBodyOutcome::Done { output, intents } => {
                match declaration.admits(OutcomeShape::Done { intents: &intents }) {
                    Ok(()) => Ok(SingletonCapture::Done { output, intents }),
                    Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                }
            }
            SingletonBodyOutcome::Failed { output } => Ok(SingletonCapture::Failed { output }),
        };
        let (result, materials) = match capture {
            Err(source) => (AttemptResult::Deferred { source }, Vec::new()),
            Ok(capture) => {
                let done = matches!(capture, SingletonCapture::Done { .. });
                let (output, entry) = mint(&owner, MaterialRole::AttemptOutput, encode(&capture)?)?;
                let result = if done {
                    AttemptResult::Done { output }
                } else {
                    AttemptResult::Failed {
                        output,
                        retryable: false,
                    }
                };
                (result, vec![entry])
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
        _ => Err(RunEventRefusal::BoundaryOrder {
            call_id: call.call_id.clone(),
        }
        .into()),
    }
}
