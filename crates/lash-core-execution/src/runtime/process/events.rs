pub use lash_core_store::process_identity::*;
use lash_sansio::{CancelOrigin, CancelRequest};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::effect_summary::{
    PROCESS_EFFECT_OMISSIONS_EVENT_TYPE, PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
    ProcessEffectOccurrence, ProcessEffectOmissions,
};
use super::model::{ProcessExternalRef, ProcessId, ProcessObserverBy, ProcessStarted, WaitState};

/// Who wrote an [`ProcessStatus::Abandoned`] terminal (ADR 0110).
///
/// The engine owns recovery, so lash itself writes an abandonment only when
/// it refuses to resume work it cannot replay. Every other abandonment is the
/// engine's own recorded outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbandonWriter {
    /// The process's engine recorded that its work was lost.
    Producer,
    /// The resume fence refused to run a started process, before any effect,
    /// because it cannot be resumed safely (FIG-3588). `reason` says why.
    ResumeRefused { reason: ProcessResumeRefusal },
}

/// Why a started process cannot be resumed safely.
///
/// One vocabulary for every "cannot resume" terminal, whichever substrate or
/// engine decides it. Each reason is decided before the run issues any effect
/// and ends the process [`ProcessStatus::Abandoned`] with
/// [`AbandonWriter::ResumeRefused`] evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessResumeRefusal {
    /// The process's executable was written by a retired generation, so this
    /// build cannot run it (FIG-3571). `found` names the stored identity the
    /// run refused (a Lashlang process names its module ref), so a later
    /// drain or migration can find what was refused.
    RetiredGeneration { found: String },
    /// Stored executable bytes failed validation; another generation cannot repair them.
    StoredArtifactCorrupt {
        artifact_ref: String,
        source: crate::ModuleArtifactCorruption,
    },
}

/// Evidence attached to an [`ProcessStatus::Abandoned`] terminal: which
/// path wrote it, the owner identity it was established against (absent
/// when no owner was established), and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbandonEvidence {
    pub writer: AbandonWriter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<crate::LeaseOwnerIdentity>,
    pub epoch_ms: u64,
}

/// Authority under which a terminal completion
/// ([`ProcessRegistry::complete_process`](super::registry::ProcessRegistry::complete_process))
/// is written.
///
/// Each variant names the engine whose single-writer discipline authorizes
/// the completion. In-process Rust cannot make such a
/// token unforgeable; the value of
/// this type is instead **explicitness + a single validation choke point per
/// backend + audit evidence** on the terminal write. Every backend calls
/// [`validate`](Self::validate) against the row's ownership (its input class)
/// inside its completion operation, and records the
/// authority on the durable terminal event (see [`terminal_append_request`]).
///
/// There is deliberately no `Default`: a caller must name its authority, the
/// same footgun-prevention stance the runtime takes elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case")]
pub enum ProcessCompletionAuthority {
    /// An engine that coalesces a process's executions under one workflow key
    /// (the `process_id`) completes a row it ran itself. Its single-writer discipline is the
    /// engine's per-key coalescing; `workflow_key` records the
    /// key that served as that discipline. Valid for every process: lash
    /// executes every process it registers.
    WorkflowKey { workflow_key: String },
    /// A workflow-key substrate ends a row that segment `segment_ordinal`
    /// could not resume (a recovery that found the segment's journal lost).
    /// Refused typed ([`crate::PluginError::ProcessHandedOver`]) when a later
    /// segment already carries the row: the external reference names that
    /// segment, and the check runs in the transaction that would append the
    /// terminal, so a recovery and a handover to a later segment exclude each
    /// other (FIG-3820).
    WorkflowKeyRecovery {
        workflow_key: String,
        segment_ordinal: u64,
    },
    /// The process's own actor ends it in its terminal transaction (ADR
    /// 0132 §11). Its single-writer discipline is the actor's epoch fence,
    /// checked by the same commit; `epoch` records the epoch it held.
    ActorEpoch { epoch: u64 },
}

impl ProcessCompletionAuthority {
    pub fn workflow_key(workflow_key: impl Into<String>) -> Self {
        Self::WorkflowKey {
            workflow_key: workflow_key.into(),
        }
    }

    /// Short, stable label for diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkflowKey { .. } => "workflow-key",
            Self::WorkflowKeyRecovery { .. } => "workflow-key-recovery",
            Self::ActorEpoch { .. } => "actor-epoch",
        }
    }

    /// Validate this authority against the row. This is the single
    /// per-backend choke point that keeps completion authority honest: each
    /// `complete_process` implementation calls it before appending the
    /// terminal event, so the contract is enforced uniformly across SQLite and
    /// Postgres rather than at each scattered caller.
    pub fn validate(&self, record: &super::ProcessRecord) -> Result<(), crate::PluginError> {
        if let Self::WorkflowKeyRecovery {
            segment_ordinal, ..
        } = self
            && let Some(carrier) = record
                .external_ref
                .as_ref()
                .map(super::ProcessExternalRef::segment_ordinal)
                .filter(|carrier| carrier > segment_ordinal)
        {
            return Err(crate::PluginError::ProcessHandedOver {
                process_id: record.id.clone(),
                segment_ordinal: carrier,
            });
        }
        Ok(())
    }
}

/// The kind of one process lifecycle fact: the closed vocabulary a process's
/// event log is written in. Nothing outside lash declares a kind, so a stored
/// event names one of these or is refused when it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum ProcessEventKind {
    #[serde(rename = "process.first_started")]
    FirstStarted,
    #[serde(rename = "process.waiting")]
    Waiting,
    #[serde(rename = "process.resumed")]
    Resumed,
    #[serde(rename = "process.effect_outcome")]
    EffectOutcome,
    #[serde(rename = "process.effect_omissions")]
    EffectOmissions,
    #[serde(rename = "process.cancel_requested")]
    CancelRequested,
    #[serde(rename = "process.observer_added")]
    ObserverAdded,
    #[serde(rename = "process.observer_removed")]
    ObserverRemoved,
    #[serde(rename = "process.external_ref_set")]
    ExternalRefSet,
    #[serde(rename = "process.completed")]
    Completed,
    #[serde(rename = "process.failed")]
    Failed,
    #[serde(rename = "process.cancelled")]
    Cancelled,
    #[serde(rename = "process.abandoned")]
    Abandoned,
}

impl ProcessEventKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 13] = [
        Self::FirstStarted,
        Self::Waiting,
        Self::Resumed,
        Self::EffectOutcome,
        Self::EffectOmissions,
        Self::CancelRequested,
        Self::ObserverAdded,
        Self::ObserverRemoved,
        Self::ExternalRefSet,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::Abandoned,
    ];

    /// The stored spelling: the event's type column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstStarted => "process.first_started",
            Self::Waiting => "process.waiting",
            Self::Resumed => "process.resumed",
            Self::EffectOutcome => PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
            Self::EffectOmissions => PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
            Self::CancelRequested => "process.cancel_requested",
            Self::ObserverAdded => "process.observer_added",
            Self::ObserverRemoved => "process.observer_removed",
            Self::ExternalRefSet => "process.external_ref_set",
            Self::Completed => "process.completed",
            Self::Failed => "process.failed",
            Self::Cancelled => "process.cancelled",
            Self::Abandoned => "process.abandoned",
        }
    }

    /// The kind stored as `event_type`; `None` for anything else.
    pub fn parse(event_type: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == event_type)
    }

    /// The kind of the terminal event that ends a process in `status`.
    pub const fn for_terminal(status: TerminalProcessStatus) -> Self {
        match status {
            TerminalProcessStatus::Completed => Self::Completed,
            TerminalProcessStatus::Failed => Self::Failed,
            TerminalProcessStatus::Cancelled => Self::Cancelled,
            TerminalProcessStatus::Abandoned => Self::Abandoned,
        }
    }

    /// The status a terminal kind ends its process in; `None` for every
    /// other kind.
    pub const fn terminal_status(self) -> Option<TerminalProcessStatus> {
        match self {
            Self::Completed => Some(TerminalProcessStatus::Completed),
            Self::Failed => Some(TerminalProcessStatus::Failed),
            Self::Cancelled => Some(TerminalProcessStatus::Cancelled),
            Self::Abandoned => Some(TerminalProcessStatus::Abandoned),
            _ => None,
        }
    }

    /// Whether only the runtime, under its execution authority, appends
    /// this kind: the effect summary and observer membership, which a host
    /// append must not pre-empt.
    pub const fn is_runtime_owned(self) -> bool {
        matches!(
            self,
            Self::EffectOutcome
                | Self::EffectOmissions
                | Self::ObserverAdded
                | Self::ObserverRemoved
        )
    }

    /// The JSON Schema of this kind's payload, for the kinds the runtime
    /// publishes one for: the effect summary.
    pub fn payload_schema(self) -> Option<crate::JsonSchema> {
        match self {
            Self::EffectOutcome => Some(super::effect_summary::effect_outcome_payload_schema()),
            Self::EffectOmissions => Some(super::effect_summary::effect_omissions_payload_schema()),
            _ => None,
        }
    }
}

impl std::fmt::Display for ProcessEventKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One fact of a process's lifecycle: what a process event records. The
/// vocabulary is closed and every fact is typed; a terminal is the typed
/// outcome the process ended in.
///
/// It is stored as its kind's spelling ([`Self::event_type`]) and a payload
/// ([`Self::payload`]), and read back through [`Self::decode`].
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessLifecycleFact {
    /// An execution attempt started.
    Started { started: ProcessStarted },
    /// The process parked on a wait.
    Waiting { wait: WaitState },
    /// The wait the process was parked on ended.
    Resumed { wait: WaitState },
    /// One recorded effect occurrence (ADR 0100 R4).
    EffectOutcome(ProcessEffectOccurrence),
    /// The effect occurrences past the per-node cap, counted.
    EffectOmissions(ProcessEffectOmissions),
    /// The process's cancel was requested.
    CancelRequested(CancelRequest),
    /// A session began observing the process.
    ObserverAdded {
        session: crate::SessionId,
        by: ProcessObserverBy,
    },
    /// A session stopped observing the process.
    ObserverRemoved {
        session: crate::SessionId,
        by: ProcessObserverBy,
    },
    /// The process was bound to durable backend work.
    ExternalRefSet { external_ref: ProcessExternalRef },
    /// The process ended in `outcome`, written under `authority`.
    Terminal {
        outcome: ProcessTerminal,
        authority: Option<ProcessCompletionAuthority>,
    },
}

impl ProcessLifecycleFact {
    /// This fact's kind.
    pub fn kind(&self) -> ProcessEventKind {
        match self {
            Self::Started { .. } => ProcessEventKind::FirstStarted,
            Self::Waiting { .. } => ProcessEventKind::Waiting,
            Self::Resumed { .. } => ProcessEventKind::Resumed,
            Self::EffectOutcome(_) => ProcessEventKind::EffectOutcome,
            Self::EffectOmissions(_) => ProcessEventKind::EffectOmissions,
            Self::CancelRequested(_) => ProcessEventKind::CancelRequested,
            Self::ObserverAdded { .. } => ProcessEventKind::ObserverAdded,
            Self::ObserverRemoved { .. } => ProcessEventKind::ObserverRemoved,
            Self::ExternalRefSet { .. } => ProcessEventKind::ExternalRefSet,
            Self::Terminal { outcome, .. } => ProcessEventKind::for_terminal(outcome.status()),
        }
    }

    /// The stored spelling of this fact's kind.
    pub fn event_type(&self) -> &'static str {
        self.kind().as_str()
    }

    /// The outcome a terminal fact ends its process in.
    pub fn terminal(&self) -> Option<&ProcessTerminal> {
        match self {
            Self::Terminal { outcome, .. } => Some(outcome),
            _ => None,
        }
    }

    /// This fact's stored payload.
    #[expect(
        clippy::expect_used,
        reason = "every lifecycle payload is a crate-owned serializable struct"
    )]
    pub fn payload(&self) -> serde_json::Value {
        match self {
            Self::Started { started } => serde_json::json!({ "started": started }),
            Self::Waiting { wait } | Self::Resumed { wait } => serde_json::json!({ "wait": wait }),
            Self::EffectOutcome(occurrence) => {
                serde_json::to_value(occurrence).expect("an effect occurrence serializes")
            }
            Self::EffectOmissions(omissions) => {
                serde_json::to_value(omissions).expect("effect omissions serialize")
            }
            Self::CancelRequested(request) => {
                serde_json::to_value(request).expect("a cancel request serializes")
            }
            Self::ObserverAdded { session, by } | Self::ObserverRemoved { session, by } => {
                serde_json::json!({ "session": session, "by": by })
            }
            Self::ExternalRefSet { external_ref } => {
                serde_json::json!({ "external_ref": external_ref })
            }
            Self::Terminal { outcome, authority } => {
                let mut payload = serde_json::json!({ "await_output": outcome });
                if let Some(authority) = authority {
                    payload["completion_authority"] =
                        serde_json::to_value(authority).expect("completion authority serializes");
                }
                payload
            }
        }
    }

    /// Read a stored fact back from its kind's spelling and its payload.
    ///
    /// # Errors
    ///
    /// [`crate::PluginError::ReservedProcessEvent`] for a spelling that names
    /// no lifecycle kind, and a session error for a payload its kind refuses.
    pub fn decode(
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<Self, crate::PluginError> {
        let kind = ProcessEventKind::parse(event_type).ok_or_else(|| {
            crate::PluginError::ReservedProcessEvent {
                event_type: event_type.to_string(),
            }
        })?;
        let invalid = |error: serde_json::Error| {
            crate::PluginError::Session(format!(
                "process event `{event_type}` has an invalid payload: {error}"
            ))
        };
        Ok(match kind {
            ProcessEventKind::FirstStarted => {
                let fields: StartedPayload = serde_json::from_value(payload).map_err(invalid)?;
                Self::Started {
                    started: fields.started,
                }
            }
            ProcessEventKind::Waiting => Self::Waiting {
                wait: serde_json::from_value::<WaitPayload>(payload)
                    .map_err(invalid)?
                    .wait,
            },
            ProcessEventKind::Resumed => Self::Resumed {
                wait: serde_json::from_value::<WaitPayload>(payload)
                    .map_err(invalid)?
                    .wait,
            },
            ProcessEventKind::EffectOutcome => {
                Self::EffectOutcome(serde_json::from_value(payload).map_err(invalid)?)
            }
            ProcessEventKind::EffectOmissions => {
                Self::EffectOmissions(serde_json::from_value(payload).map_err(invalid)?)
            }
            ProcessEventKind::CancelRequested => {
                Self::CancelRequested(serde_json::from_value(payload).map_err(invalid)?)
            }
            ProcessEventKind::ObserverAdded => {
                let ObserverPayload { session, by } =
                    serde_json::from_value(payload).map_err(invalid)?;
                Self::ObserverAdded { session, by }
            }
            ProcessEventKind::ObserverRemoved => {
                let ObserverPayload { session, by } =
                    serde_json::from_value(payload).map_err(invalid)?;
                Self::ObserverRemoved { session, by }
            }
            ProcessEventKind::ExternalRefSet => Self::ExternalRefSet {
                external_ref: serde_json::from_value::<ExternalRefPayload>(payload)
                    .map_err(invalid)?
                    .external_ref,
            },
            ProcessEventKind::Completed
            | ProcessEventKind::Failed
            | ProcessEventKind::Cancelled
            | ProcessEventKind::Abandoned => {
                let TerminalPayload {
                    await_output,
                    completion_authority,
                } = serde_json::from_value(payload).map_err(invalid)?;
                let declared = kind.terminal_status();
                let outcome = ProcessTerminal::try_from(await_output).map_err(|_| {
                    crate::PluginError::ProcessTerminalOutcomeMismatch {
                        declared_status: declared.map_or(ProcessStatus::Completed, Into::into),
                        outcome_status: None,
                    }
                })?;
                if Some(outcome.status()) != declared {
                    return Err(crate::PluginError::ProcessTerminalOutcomeMismatch {
                        declared_status: declared.map_or(ProcessStatus::Completed, Into::into),
                        outcome_status: Some(outcome.status().into()),
                    });
                }
                Self::Terminal {
                    outcome,
                    authority: completion_authority,
                }
            }
        })
    }

    /// Whether `other` is the same fact for a replay: equal, with a cancel
    /// request compared on its cancellation rather than its clock.
    pub fn same_fact(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::CancelRequested(left), Self::CancelRequested(right)) => {
                left.same_cancellation_as(right)
            }
            _ => {
                crate::identity_json::payloads_equal(&self.payload(), &other.payload())
                    && self.kind() == other.kind()
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedPayload {
    started: ProcessStarted,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitPayload {
    wait: WaitState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserverPayload {
    session: crate::SessionId,
    by: ProcessObserverBy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalRefPayload {
    external_ref: ProcessExternalRef,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalPayload {
    await_output: ProcessAwaitOutput,
    #[serde(default)]
    completion_authority: Option<ProcessCompletionAuthority>,
}

/// The replay-keyed terminal event append for a completion.
///
/// The single source of truth for the terminal event's kind, replay key, and
/// payload, shared by every completion path across all backends. A completion
/// records its authority beside its outcome as durable audit evidence.
pub fn terminal_append_request(
    process_id: &ProcessId,
    await_output: &ProcessAwaitOutput,
    authority: Option<&ProcessCompletionAuthority>,
) -> ProcessEventAppendRequest {
    #[expect(
        clippy::expect_used,
        reason = "only terminal outcomes reach this append"
    )]
    let outcome = ProcessTerminal::try_from(await_output.clone())
        .expect("only terminal outcomes may be appended");
    let event_type = ProcessEventKind::for_terminal(outcome.status()).as_str();
    ProcessEventAppendRequest::new(ProcessLifecycleFact::Terminal {
        outcome,
        authority: authority.cloned(),
    })
    .with_replay_key(format!("process:{process_id}:terminal:{event_type}"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessAwaitOutput {
    Settled {
        output: Box<crate::ToolCallOutput>,
    },
    /// The owner stopped executing without recording an outcome. Written only by
    /// a recovery pass or an owner's graceful drain, never round-tripped from a tool
    /// (a tool cannot self-report abandonment); see [`AbandonEvidence`]. The
    /// evidence is boxed so this rare terminal does not enlarge the pervasive
    /// `ProcessAwaitOutput` that flows through every tool result.
    Abandoned {
        evidence: Box<AbandonEvidence>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<crate::ToolControl>,
    },
    NoLongerRetained {
        terminal_label: RetiredProcessStatus,
        pruned_at_ms: u64,
    },
}

impl ProcessAwaitOutput {
    /// Stamps a cancelled result with the standing process request's origin.
    /// Other outcomes and cancellation payloads with no standing request retain
    /// their original representation. Registries apply this before completion
    /// replay comparison and event identity construction.
    pub fn with_cancel_origin(mut self, origin: Option<crate::CancelOrigin>) -> Self {
        if let Some(origin) = origin
            && let Self::Settled { output } = &mut self
            && let crate::ToolCallOutcome::Cancelled(cancellation) = &mut output.outcome
        {
            cancellation.origin = Some(origin);
        }
        self
    }

    /// Projects only terminal process outcomes to their durable status for store implementors,
    /// returning `None` for an answer that is not an outcome.
    pub fn terminal_status(&self) -> Option<TerminalProcessStatus> {
        match self {
            Self::Settled { output } => Some(settled_status(output)),
            Self::Abandoned { .. } => Some(TerminalProcessStatus::Abandoned),
            Self::NoLongerRetained { .. } => None,
        }
    }

    /// Builds a `ProcessAwaitOutput` from tool output data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_tool_output(output: crate::ToolCallOutput) -> Self {
        Self::Settled {
            output: Box::new(output),
        }
    }

    /// Extracts the tool output outcome for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    pub fn into_tool_output(self) -> crate::ToolCallOutput {
        match self {
            Self::Settled { output } => *output,
            // Abandonment has no `ToolCallOutcome` peer: a tool never self-reports
            // it. To a caller awaiting the result it surfaces one-directionally as
            // an external failure whose raw payload names it abandoned and carries
            // the evidence, while the process layer keeps `Abandoned` a distinct
            // terminal (ADR 0110). `from_tool_output` therefore never reverses this.
            Self::Abandoned { evidence, control } => {
                let raw = serde_json::to_value(&evidence)
                    .ok()
                    .map(crate::ToolValue::untrusted_json);
                let message = match evidence.writer {
                    AbandonWriter::Producer => {
                        "process abandoned: its producer recorded the work as lost".to_string()
                    }
                    AbandonWriter::ResumeRefused {
                        reason: ProcessResumeRefusal::RetiredGeneration { found },
                    } => format!(
                        "process abandoned: its executable `{found}` was written by a retired \
                         generation"
                    ),
                    AbandonWriter::ResumeRefused {
                        reason:
                            ProcessResumeRefusal::StoredArtifactCorrupt {
                                artifact_ref,
                                source,
                            },
                    } => format!(
                        "process abandoned: stored artifact `{artifact_ref}` is corrupt: {source}"
                    ),
                };
                let mut failure = crate::ToolFailure::tool(
                    crate::ToolFailureClass::External,
                    "process_abandoned",
                    message,
                );
                failure.raw = raw;
                let mut output = crate::ToolCallOutput::failure(failure);
                output.control = control;
                output
            }
            // The outcome was pruned, the status it ended in was not: only a
            // process that completed answers a success, and every other
            // retired status answers the failure or cancellation it was.
            Self::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            } => {
                let detail = serde_json::json!({
                    "terminal_label": terminal_label,
                    "pruned_at_ms": pruned_at_ms,
                });
                let failure = |class, message: &str| {
                    let mut failure =
                        crate::ToolFailure::runtime(class, "process_no_longer_retained", message);
                    failure.raw = Some(crate::ToolValue::untrusted_json(detail.clone()));
                    crate::ToolCallOutput::failure(failure)
                };
                match terminal_label {
                    RetiredProcessStatus::Completed => {
                        crate::ToolCallOutput::success(serde_json::json!({
                            "type": "information",
                            "code": "process_no_longer_retained",
                            "message": "process completed, but its outcome is no longer retained",
                            "terminal_label": terminal_label,
                            "pruned_at_ms": pruned_at_ms,
                        }))
                    }
                    RetiredProcessStatus::Failed => failure(
                        crate::ToolFailureClass::Execution,
                        "process failed, and its outcome is no longer retained",
                    ),
                    RetiredProcessStatus::Cancelled => {
                        let mut cancellation = crate::ToolCancellation::runtime(
                            "process was cancelled, and its outcome is no longer retained",
                        );
                        cancellation.raw = Some(crate::ToolValue::untrusted_json(detail));
                        crate::ToolCallOutput::cancelled(cancellation)
                    }
                    RetiredProcessStatus::Abandoned => failure(
                        crate::ToolFailureClass::External,
                        "process was abandoned, and its evidence is no longer retained",
                    ),
                }
            }
        }
    }
}

fn settled_status(output: &crate::ToolCallOutput) -> TerminalProcessStatus {
    match &output.outcome {
        crate::ToolCallOutcome::Success(_) => TerminalProcessStatus::Completed,
        crate::ToolCallOutcome::Failure(_) => TerminalProcessStatus::Failed,
        crate::ToolCallOutcome::Cancelled(_) => TerminalProcessStatus::Cancelled,
    }
}

/// The outcome a process ended in: what a terminal record and a terminal
/// event hold.
///
/// It is the part of [`ProcessAwaitOutput`] that is an outcome, so its status
/// is a total function of it ([`Self::status`]) and is stored nowhere beside
/// it. It serializes exactly as the [`ProcessAwaitOutput`] it is.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessTerminal {
    Settled {
        output: Box<crate::ToolCallOutput>,
    },
    /// See [`ProcessAwaitOutput::Abandoned`].
    Abandoned {
        evidence: Box<AbandonEvidence>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<crate::ToolControl>,
    },
}

impl ProcessTerminal {
    /// The status this outcome ends a process in.
    pub fn status(&self) -> TerminalProcessStatus {
        match self {
            Self::Settled { output } => settled_status(output),
            Self::Abandoned { .. } => TerminalProcessStatus::Abandoned,
        }
    }

    /// Builds the outcome of a process whose work answered `output`.
    pub fn from_tool_output(output: crate::ToolCallOutput) -> Self {
        Self::Settled {
            output: Box::new(output),
        }
    }

    /// This outcome as an await answers it.
    pub fn into_await_output(self) -> ProcessAwaitOutput {
        self.into()
    }

    /// Stamps a cancelled outcome with the standing process request's
    /// origin ([`ProcessAwaitOutput::with_cancel_origin`]).
    pub fn with_cancel_origin(mut self, origin: Option<crate::CancelOrigin>) -> Self {
        if let Some(origin) = origin
            && let Self::Settled { output } = &mut self
            && let crate::ToolCallOutcome::Cancelled(cancellation) = &mut output.outcome
        {
            cancellation.origin = Some(origin);
        }
        self
    }
}

impl From<ProcessTerminal> for ProcessAwaitOutput {
    fn from(terminal: ProcessTerminal) -> Self {
        match terminal {
            ProcessTerminal::Settled { output } => Self::Settled { output },
            ProcessTerminal::Abandoned { evidence, control } => {
                Self::Abandoned { evidence, control }
            }
        }
    }
}

/// An await answer that is not an outcome: its process was pruned, and only
/// the status it retired in remains.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "a pruned process (`{terminal_label}`, pruned at {pruned_at_ms}ms) has no retained outcome"
)]
pub struct ProcessOutcomeNotRetained {
    pub terminal_label: RetiredProcessStatus,
    pub pruned_at_ms: u64,
}

impl TryFrom<ProcessAwaitOutput> for ProcessTerminal {
    type Error = ProcessOutcomeNotRetained;

    fn try_from(output: ProcessAwaitOutput) -> Result<Self, Self::Error> {
        match output {
            ProcessAwaitOutput::Settled { output } => Ok(Self::Settled { output }),
            ProcessAwaitOutput::Abandoned { evidence, control } => {
                Ok(Self::Abandoned { evidence, control })
            }
            ProcessAwaitOutput::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            } => Err(ProcessOutcomeNotRetained {
                terminal_label,
                pruned_at_ms,
            }),
        }
    }
}

impl<'de> Deserialize<'de> for ProcessTerminal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::try_from(ProcessAwaitOutput::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

/// One stored process event: a lifecycle fact at its place in the process's
/// log.
///
/// It is stored as the fact's kind spelling and payload beside the rest, and
/// a row whose fact does not decode is refused when it is read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "ProcessEventRecord", try_from = "ProcessEventRecord")]
pub struct ProcessEvent {
    pub process_id: ProcessId,
    pub sequence: u64,
    pub fact: ProcessLifecycleFact,
    pub invocation: crate::RuntimeInvocation,
    /// What caused the event, as the append that inserted it retained it.
    /// Written once with the event; a replayed append reads it back whatever
    /// it carried.
    pub trace_cause: lash_trace::TraceCause,
    pub occurred_at: u64,
}

impl ProcessEvent {
    /// The event's kind.
    pub fn kind(&self) -> ProcessEventKind {
        self.fact.kind()
    }

    /// The outcome a terminal event ends its process in.
    pub fn terminal(&self) -> Option<&ProcessTerminal> {
        self.fact.terminal()
    }
}

/// The stored form of a [`ProcessEvent`]: its fact as its kind's spelling and
/// payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessEventRecord {
    process_id: ProcessId,
    sequence: u64,
    event_type: String,
    payload: serde_json::Value,
    invocation: crate::RuntimeInvocation,
    #[serde(default, skip_serializing_if = "lash_trace::TraceCause::is_root")]
    trace_cause: lash_trace::TraceCause,
    occurred_at: u64,
}

impl From<ProcessEvent> for ProcessEventRecord {
    fn from(event: ProcessEvent) -> Self {
        Self {
            process_id: event.process_id,
            sequence: event.sequence,
            event_type: event.fact.event_type().to_string(),
            payload: event.fact.payload(),
            invocation: event.invocation,
            trace_cause: event.trace_cause,
            occurred_at: event.occurred_at,
        }
    }
}

impl TryFrom<ProcessEventRecord> for ProcessEvent {
    type Error = crate::PluginError;

    fn try_from(record: ProcessEventRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            fact: ProcessLifecycleFact::decode(&record.event_type, record.payload)?,
            process_id: record.process_id,
            sequence: record.sequence,
            invocation: record.invocation,
            trace_cause: record.trace_cause,
            occurred_at: record.occurred_at,
        })
    }
}

/// Payload projection selected for a bounded process-event page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessEventQueryMode {
    /// Return the complete durable event, including its JSON payload.
    Full,
    /// Return only the event ordering position and type.
    Lite,
}

/// Event metadata returned by the payload-free SQL projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessEventLite {
    pub sequence: u64,
    pub kind: ProcessEventKind,
}

/// Projection-specific contents of one bounded event page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "mode",
    content = "events",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProcessEventPageEvents<Full = ProcessEvent, Lite = ProcessEventLite> {
    Full(Vec<Full>),
    Lite(Vec<Lite>),
}

impl<Full, Lite> ProcessEventPageEvents<Full, Lite> {
    pub fn len(&self) -> usize {
        match self {
            Self::Full(events) => events.len(),
            Self::Lite(events) => events.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whether a bounded event page completed the retained history.
///
/// A host continues through the process cursor the facade returns with the
/// page; the registry-level boundary is the last returned sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessEventPageMore {
    Complete,
    More {
        /// The exclusive sequence boundary the next page starts after.
        after_sequence: u64,
    },
}

/// One bounded page from a retained process-event history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessEventPage<Full = ProcessEvent, Lite = ProcessEventLite> {
    pub events: ProcessEventPageEvents<Full, Lite>,
    pub more: ProcessEventPageMore,
}

impl ProcessEventPage {
    pub fn from_full_rows(mut events: Vec<ProcessEvent>, limit: std::num::NonZeroUsize) -> Self {
        let more = page_more(&mut events, limit, |event| event.sequence);
        Self {
            events: ProcessEventPageEvents::Full(events),
            more,
        }
    }

    pub fn from_lite_rows(
        mut events: Vec<ProcessEventLite>,
        limit: std::num::NonZeroUsize,
    ) -> Self {
        let more = page_more(&mut events, limit, |event| event.sequence);
        Self {
            events: ProcessEventPageEvents::Lite(events),
            more,
        }
    }
}

impl<Full, Lite> ProcessEventPage<Full, Lite> {
    /// The highest sequence this page returned, if it returned any event.
    pub fn last_sequence(
        &self,
        full_sequence: impl Fn(&Full) -> u64,
        lite_sequence: impl Fn(&Lite) -> u64,
    ) -> Option<u64> {
        match &self.events {
            ProcessEventPageEvents::Full(events) => events.last().map(full_sequence),
            ProcessEventPageEvents::Lite(events) => events.last().map(lite_sequence),
        }
    }
}

fn page_more<T>(
    rows: &mut Vec<T>,
    limit: std::num::NonZeroUsize,
    sequence: impl Fn(&T) -> u64,
) -> ProcessEventPageMore {
    if rows.len() <= limit.get() {
        return ProcessEventPageMore::Complete;
    }
    rows.truncate(limit.get());
    let Some(last) = rows.last() else {
        return ProcessEventPageMore::Complete;
    };
    ProcessEventPageMore::More {
        after_sequence: sequence(last),
    }
}

/// Why the exact process history named by a page read is no longer retained.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessEventHistoryRetention {
    /// The process was pruned and its payload-free tombstone is still present.
    Pruned {
        terminal_label: RetiredProcessStatus,
        pruned_at_ms: u64,
    },
    /// The host released the process's events at or below `released_through`
    /// ([`ProcessRetention::release_process_events`](super::ProcessRetention::release_process_events)).
    /// The process is retained; a reader resumes strictly after the horizon.
    Released { released_through: u64 },
}

/// What one host release of a process's event prefix settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessEventRelease {
    /// The process's release horizon after the call: every event at or below
    /// it is released. Never lower than before the call, and never above the
    /// process's last event.
    pub released_through: u64,
    /// The events this call released. Zero when the prefix was already
    /// released, so repeated cleanup reports nothing new.
    pub released_events: u64,
}

/// The payload digest a released event keeps in place of its payload: the
/// SHA-256 of the payload's identity leaf, so a re-presented append under the
/// event's replay key is matched on the same bytes [`ProcessEventAppendRequest`]
/// replay matching compares.
fn released_payload_digest(payload: &serde_json::Value) -> String {
    crate::stable_hash::sha256_hex(&crate::identity_json::payload_leaf(payload))
}

/// The digest a store keeps in place of `event`'s payload when it releases
/// the event (FIG-3482). Sequence, kind, replay identity and the rest of the
/// row stay, so sequence allocation and the replay-key fence are exactly
/// what they were.
///
/// `None` leaves the event as it is: a cancel request matches its replays on
/// its cancellation rather than on payload bytes, and is one row per process.
pub fn release_process_event_payload(event: &ProcessEvent) -> Option<String> {
    if event.kind() == ProcessEventKind::CancelRequested {
        return None;
    }
    Some(released_payload_digest(&event.fact.payload()))
}

/// A released event's row without its payload: what a store keeps of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasedProcessEvent {
    pub process_id: ProcessId,
    pub sequence: u64,
    pub event_type: String,
    pub invocation: crate::RuntimeInvocation,
    #[serde(default, skip_serializing_if = "lash_trace::TraceCause::is_root")]
    pub trace_cause: lash_trace::TraceCause,
    pub occurred_at: u64,
}

impl ReleasedProcessEvent {
    /// The row `event` keeps once its payload is released.
    pub fn of(event: &ProcessEvent) -> Self {
        Self {
            process_id: event.process_id.clone(),
            sequence: event.sequence,
            event_type: event.fact.event_type().to_string(),
            invocation: event.invocation.clone(),
            trace_cause: event.trace_cause.clone(),
            occurred_at: event.occurred_at,
        }
    }
}

/// Rebuild a released event found under a re-presented replay key for the
/// append's replay match: a request of the same kind whose payload has the
/// released digest gets its fact back on the row, so the replay answers the
/// recorded event; any other request is the same durable-identity conflict a
/// retained event refuses.
pub fn restore_released_process_event(
    released: ReleasedProcessEvent,
    released_digest: &str,
    requested: &ProcessEventAppendRequest,
) -> Result<ProcessEvent, crate::PluginError> {
    if released.event_type == requested.fact.event_type()
        && released_payload_digest(&requested.fact.payload()) == released_digest
    {
        return Ok(ProcessEvent {
            process_id: released.process_id,
            sequence: released.sequence,
            fact: requested.fact.clone(),
            invocation: released.invocation,
            trace_cause: released.trace_cause,
            occurred_at: released.occurred_at,
        });
    }
    Err(crate::durable_identity_conflict(format!(
        "process `{}` event replay key conflicts with released event {}",
        released.process_id, released.sequence
    )))
}

/// Result of reading a process-event history without collapsing retention into
/// an empty page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "retention",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProcessEventReadOutcome<Page = ProcessEventPage> {
    Retained(Page),
    NoLongerRetained(ProcessEventHistoryRetention),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessEventAppendReceipt {
    pub event: ProcessEvent,
    /// Sequence durably folded into the process record when this append
    /// settled. On replay this can be newer than `event.sequence`.
    pub last_event_sequence: u64,
    /// Whether this call wrote the event row, or the store found the same
    /// replay key already appended and returned the recorded event (FIG-3070).
    ///
    /// This is the [`ProcessEventAppendPlan`](super::validation::ProcessEventAppendPlan)
    /// arm the store took, carried out to the caller instead of being discarded
    /// at the store boundary. Defaulted and omitted when `Realized` so the
    /// receipt's encoding is unchanged for anything that round-trips it.
    #[serde(default, skip_serializing_if = "crate::StoreRealization::is_realized")]
    pub realization: crate::StoreRealization,
}

/// One lifecycle fact to append, under its deterministic replay key.
///
/// Built only through the typed constructors: every fact a process's log
/// holds is one lash records.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessEventAppendRequest {
    pub fact: ProcessLifecycleFact,
    pub replay: Option<crate::RuntimeReplay>,
    /// What caused the event, for telemetry: the append that inserts the
    /// event retains it on the event. It takes no part in the append's replay
    /// identity or its payload match.
    pub trace_cause: lash_trace::TraceCause,
}

impl ProcessEventAppendRequest {
    fn new(fact: ProcessLifecycleFact) -> Self {
        Self {
            fact,
            replay: None,
            trace_cause: lash_trace::TraceCause::Root,
        }
    }

    /// Read a stored append back: the store's `AppendEvent` write carries
    /// the fact's kind spelling, its payload and its replay key.
    ///
    /// # Errors
    ///
    /// A spelling or payload [`ProcessLifecycleFact::decode`] refuses, or an
    /// empty replay key: every lifecycle append is keyed.
    pub fn from_stored(
        event_type: &str,
        payload: serde_json::Value,
        replay_key: impl Into<String>,
    ) -> Result<Self, crate::PluginError> {
        let replay_key = replay_key.into();
        if replay_key.is_empty() {
            return Err(crate::PluginError::Session(format!(
                "process event `{event_type}` requires a deterministic replay key"
            )));
        }
        Ok(
            Self::new(ProcessLifecycleFact::decode(event_type, payload)?)
                .with_replay_key(replay_key),
        )
    }

    /// The kind of the fact this append records.
    pub fn kind(&self) -> ProcessEventKind {
        self.fact.kind()
    }

    /// Attaches a replay key for process-store implementors so an equivalent event append is
    /// idempotent within that key.
    pub fn with_replay_key(mut self, replay_key: impl Into<String>) -> Self {
        self.replay = Some(crate::RuntimeReplay {
            key: replay_key.into(),
            attribution: None,
        });
        self
    }

    /// Sets the optional replay carried by a `ProcessEventAppendRequest` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_optional_replay(mut self, replay: Option<crate::RuntimeReplay>) -> Self {
        self.replay = replay;
        self
    }

    /// Sets what caused the event ([`Self::trace_cause`]).
    pub fn with_trace_cause(mut self, trace_cause: lash_trace::TraceCause) -> Self {
        self.trace_cause = trace_cause;
        self
    }

    /// Build a cancellation event keyed by process, origin, and requester.
    /// Retrying with a fresh clock retains the first accepted cancellation fact.
    pub fn cancel_requested(process_id: &ProcessId, request: &CancelRequest) -> Self {
        Self::new(ProcessLifecycleFact::CancelRequested(request.clone()))
            .with_replay_key(cancellation_replay_key(process_id, request))
    }

    /// Builds a first-start event for process-store implementors keyed by attempt number so a retry
    /// cannot alias the preceding execution attempt.
    pub fn first_started(process_id: &ProcessId, started: &ProcessStarted) -> Self {
        Self::new(ProcessLifecycleFact::Started {
            started: started.clone(),
        })
        .with_replay_key(format!(
            "process:{process_id}:first-started:attempt:{}",
            started.attempt
        ))
    }

    /// Builds a wait-entry event for process-store implementors keyed by wait identity and start
    /// time so replay cannot duplicate the transition.
    pub fn wait_entered(process_id: &ProcessId, wait: &WaitState) -> Self {
        Self::new(ProcessLifecycleFact::Waiting { wait: wait.clone() }).with_replay_key(format!(
            "process:{process_id}:wait:{}:since:{}:entered",
            wait.key(),
            wait.since_ms
        ))
    }

    /// Builds a wait-clear event for process-store implementors keyed to the exact wait identity
    /// and start time being resumed.
    pub fn wait_cleared(process_id: &ProcessId, wait: &WaitState) -> Self {
        Self::new(ProcessLifecycleFact::Resumed { wait: wait.clone() }).with_replay_key(format!(
            "process:{process_id}:wait:{}:since:{}:cleared",
            wait.key(),
            wait.since_ms
        ))
    }

    /// Builds the single replay-stable external-reference event for process-store implementors
    /// binding durable backend work.
    pub fn external_ref_set(process_id: &ProcessId, external_ref: &ProcessExternalRef) -> Self {
        Self::new(ProcessLifecycleFact::ExternalRefSet {
            external_ref: external_ref.clone(),
        })
        .with_replay_key(match external_ref.segment_ordinal() {
            // Segment zero keeps the key every pre-segment writer used, so an
            // existing row's recorded append identity does not move. A later
            // segment mints its own key: one key per segment is what lets a
            // superseding reference be appended at all, since a replay key
            // reused with a different payload is a conflict, not an update.
            0 => format!("process:{process_id}:external-ref"),
            ordinal => format!("process:{process_id}:external-ref:{ordinal}"),
        })
    }

    /// Builds an observer-add event for process-store implementors whose replay key includes
    /// process, session, and observer authority.
    pub fn observer_added(
        process_id: &ProcessId,
        session: &crate::SessionId,
        by: &ProcessObserverBy,
    ) -> Self {
        Self::new(ProcessLifecycleFact::ObserverAdded {
            session: session.clone(),
            by: by.clone(),
        })
        .with_replay_key(format!(
            "process:{process_id}:observer:{session}:add:{}",
            by.replay_component()
        ))
    }

    /// Builds an observer-remove event for process-store implementors whose replay key includes
    /// process, session, and observer authority.
    pub fn observer_removed(
        process_id: &ProcessId,
        session: &crate::SessionId,
        by: &ProcessObserverBy,
    ) -> Self {
        Self::new(ProcessLifecycleFact::ObserverRemoved {
            session: session.clone(),
            by: by.clone(),
        })
        .with_replay_key(format!(
            "process:{process_id}:observer:{session}:remove:{}",
            by.replay_component()
        ))
    }

    /// The runtime append of one effect occurrence, keyed by the effect's
    /// replay key.
    pub fn effect_outcome(occurrence: ProcessEffectOccurrence) -> Self {
        let replay_key = occurrence.replay_key.clone();
        Self::new(ProcessLifecycleFact::EffectOutcome(occurrence)).with_replay_key(replay_key)
    }

    /// The runtime append of a run's effect omissions, keyed by
    /// `replay_key`: one key per process run, so a redrive recovers the same
    /// event.
    pub fn effect_omissions(
        omissions: ProcessEffectOmissions,
        replay_key: impl Into<String>,
    ) -> Self {
        Self::new(ProcessLifecycleFact::EffectOmissions(omissions)).with_replay_key(replay_key)
    }
}

/// Version 3 drops the process incarnation: a minted process id names one
/// process (ADR 0107).
///
/// version_guard(
///     items(cancellation_replay_preimage),
/// )
/// version_surface = "coexist"
const PROCESS_CANCELLATION_FAMILY_VERSION: u8 = 3;

/// Permanent cancellation origin tags: TurnStopped=0, ParentEnded=1,
/// OperatorRequested=2, ModelRequested=3, StartFailed=4, RunClosing=5. Clock readings are
/// excluded: origin and requester identify the request on this process.
fn cancellation_replay_preimage(process_id: &ProcessId, request: &CancelRequest) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.process-cancellation-request",
        PROCESS_CANCELLATION_FAMILY_VERSION,
    );
    identity.string(process_id);
    identity.tag(match request.origin {
        CancelOrigin::TurnStopped => 0,
        CancelOrigin::ParentEnded => 1,
        CancelOrigin::OperatorRequested => 2,
        CancelOrigin::ModelRequested => 3,
        CancelOrigin::StartFailed => 4,
        CancelOrigin::RunClosing => 5,
    });
    identity.string(&request.requester);
    identity.finish()
}

fn cancellation_replay_key(process_id: &ProcessId, request: &CancelRequest) -> String {
    crate::stable_identity::rendered_hash(
        "process-cancellation",
        PROCESS_CANCELLATION_FAMILY_VERSION,
        &cancellation_replay_preimage(process_id, request),
    )
}

#[cfg(test)]
mod cancellation_identity_tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn cancellation_replay_identity_has_pinned_bounded_grammar() {
        let process_ref = crate::ProcessIdMint::sequential_id_for_testing(1);
        let request = CancelRequest::new(CancelOrigin::OperatorRequested, "λ".repeat(3_200), 10);
        let key = cancellation_replay_key(&process_ref, &request);
        assert_eq!(
            key.len(),
            95,
            "rendered key length must be input-independent"
        );
        assert_eq!(
            key,
            "process-cancellation:v3:blake3:88f852453064ce3fe2c7aa1c45fd73d10a4053d73cb2305ea4a656b46f34070c"
        );
        let empty = CancelRequest::new(CancelOrigin::OperatorRequested, "", 10);
        assert_eq!(
            hex(&cancellation_replay_preimage(&process_ref, &empty)),
            "6c6173682d737461626c652d6964656e74697479020300000000000000216c6173682e70726f636573732d63616e63656c6c6174696f6e2d726571756573740000000000000022705f3030303030303030303030303730303038303030303030303030303030303031020000000000000000"
        );
        let retry = CancelRequest {
            requested_at_ms: 99,
            ..request.clone()
        };
        assert_eq!(key, cancellation_replay_key(&process_ref, &retry));
        let other_origin = CancelRequest {
            origin: CancelOrigin::ModelRequested,
            ..request.clone()
        };
        assert_ne!(key, cancellation_replay_key(&process_ref, &other_origin));
        let other_process = crate::ProcessIdMint::sequential_id_for_testing(2);
        assert_ne!(key, cancellation_replay_key(&other_process, &request));
    }

    #[test]
    fn process_output_rejects_pre_change_settled_shapes_with_a_data_error() {
        let legacy_outputs = [
            serde_json::json!({
                "type": "failure",
                "class": "external",
                "code": "legacy_plugin_failure",
                "message": "the old row omitted provenance and retry status",
                "raw": {
                    "$lash_tool_value": "untrusted_json",
                    "value": {"status": 503}
                }
            }),
            serde_json::json!({
                "type": "success",
                "value": {
                    "$lash_tool_value": "untrusted_json",
                    "value": {"status": 200}
                }
            }),
            serde_json::json!({
                "type": "cancelled",
                "message": "the old row omitted provenance",
                "raw": {
                    "$lash_tool_value": "untrusted_json",
                    "value": {"status": 499}
                }
            }),
        ];

        for legacy in legacy_outputs {
            let await_error = serde_json::from_value::<ProcessAwaitOutput>(legacy.clone())
                .expect_err("pre-change process outputs must fail decode");
            assert_eq!(await_error.classify(), serde_json::error::Category::Data);
            let terminal_error = serde_json::from_value::<ProcessTerminal>(legacy)
                .expect_err("pre-change process terminals must fail decode");
            assert_eq!(terminal_error.classify(), serde_json::error::Category::Data);
        }
    }

    #[test]
    fn a_pruned_await_answers_the_status_the_process_retired_in() {
        for status in RetiredProcessStatus::ALL.iter().copied() {
            let output = ProcessAwaitOutput::NoLongerRetained {
                terminal_label: status,
                pruned_at_ms: 7,
            }
            .into_tool_output();
            match (status, &output.outcome) {
                (RetiredProcessStatus::Completed, crate::ToolCallOutcome::Success(_)) => {}
                (
                    RetiredProcessStatus::Failed | RetiredProcessStatus::Abandoned,
                    crate::ToolCallOutcome::Failure(failure),
                ) => {
                    assert_eq!(failure.code, "process_no_longer_retained");
                    assert_eq!(
                        failure.raw.clone().map(|raw| raw.to_json_value()),
                        Some(serde_json::json!({
                            "terminal_label": status.label(),
                            "pruned_at_ms": 7,
                        }))
                    );
                }
                (RetiredProcessStatus::Cancelled, crate::ToolCallOutcome::Cancelled(_)) => {}
                (status, outcome) => panic!("a pruned `{status}` process answered {outcome:?}"),
            }
        }
    }

    #[test]
    fn a_pruned_await_refuses_a_label_that_is_not_a_retired_status() {
        for label in ["running", "waiting", "finished", ""] {
            let output = serde_json::json!({
                "type": "no_longer_retained",
                "terminal_label": label,
                "pruned_at_ms": 7,
            });
            assert!(
                serde_json::from_value::<ProcessAwaitOutput>(output).is_err(),
                "`{label}` is not a status a process is pruned in"
            );
        }
    }

    #[test]
    fn process_output_rejects_malformed_tags_in_every_tool_value_arm() {
        let malformed = serde_json::json!({
            "$lash_tool_value": "attachment",
            "extra": true
        });
        let outputs = [
            serde_json::json!({
                "type": "settled",
                "output": {
                    "outcome": {"status": "success", "payload": malformed.clone()}
                }
            }),
            serde_json::json!({
                "type": "settled",
                "output": {
                    "outcome": {
                        "status": "failure",
                        "payload": {
                            "class": "execution",
                            "code": "provider_failure",
                            "message": "provider failed",
                            "source": "plugin",
                            "raw": malformed.clone()
                        }
                    }
                }
            }),
            serde_json::json!({
                "type": "settled",
                "output": {
                    "outcome": {
                        "status": "cancelled",
                        "payload": {
                            "message": "cancelled",
                            "source": "cancellation",
                            "raw": malformed
                        }
                    }
                }
            }),
        ];

        for output in outputs {
            assert!(
                serde_json::from_value::<ProcessAwaitOutput>(output).is_err(),
                "a malformed reserved tool-value tag must be rejected"
            );
        }
    }
}
