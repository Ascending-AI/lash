//! A cell's host: the parent's half of its kernel run.
//!
//! The machine parks and the broker commits. Between them stands the cell's
//! host, which says what each of the run's waits and reads is to lash:
//!
//! - a `perform` is a tool call. Its effect is one the cell's
//!   [`HostBoundary`] offers, and it is admitted as the tool's own
//!   execution, under the call identity the broker derived for it and the
//!   policy, limit and completion wait its tool declares
//!   ([`CellHost::admit`]). The cell is answered from the call's committed
//!   outcome alone ([`CellHost::outcome`]).
//!   A `perform` the cell's read plan binds is no call: it is answered
//!   inside its statement by the cell's reads ([`super::reads`]).
//! - a `print` is an observation, kept in order.
//! - clock and random are ordinary package Read operations.
//!
//! What the host holds of the cell is the calls it admitted with the
//! records of those that settled, and its prints. Every park commits it
//! beside the machine's state as the cell's HostRoot and CellEnvelope
//! ([`KernelEffects::stage_host`]): each settled call's CallOutcome is
//! installed once, and a park installs only the summary pages it changed,
//! so the activation that resumes the cell carries on with all of it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core::RuntimeExecutionContext;
use lash_core::facade_support::ToolInvocation;
use lash_core::tool_dispatch::{CellCall, CellMember, CellMembers};
use lash_core_store::checkpoint_graph::{GraphStaging, NodeId};
use lash_core_store::tool_surface::UseBinding;
use lash_kernel_doc::{Datum, ErrorDatum};

use lash_kernel_vm::{EffectRequest, Outcome};
use lash_protocol_types::sync::MutexExt;
use lash_vm_broker::kernel::graph::{FrozenRequest, Projections};
use lash_vm_broker::kernel::{
    AdmittedCall, AdmittedEffect, EFFECT_CANCELLED, HostParts, KernelEffects, datum_from_json,
    datum_to_json, outcome_of,
};
use lash_vm_broker::{MemberDraft, ParentFault};
use lash_vocabulary::names::OperationUse;
use lash_vocabulary::sites::EffectId;

use super::envelope::CellEnvelope;
use super::host_graph::{HostState, PrintEvent, ProcessEvent};

/// The error kind of a tool call that reported a failure of its own. Its
/// `data` is the tool's typed failure.
pub const TOOL_FAILED: &str = "tool_failed";
/// The error kind of a tool call the session's `max_tool_calls` refused.
pub const TOOL_CALL_LIMIT: &str = "tool_call_limit";
/// The error kind of a control call refused before its body ran. Its `data`
/// is the typed cause ([`lash_core::ToolFailureCause`]): made from a task the
/// cell spawned, made while a task is outstanding, or made after the cell's
/// one control attempt was spent.
pub const CONTROL_REFUSED: &str = "control_refused";
/// The error kind of a tool call whose arguments JSON cannot carry.
pub const TOOL_ARGUMENTS: &str = "tool_arguments";
/// The error kind of a `perform` of an effect the cell's host does not
/// offer.
pub const UNKNOWN_EFFECT: &str = "unknown_effect";

/// One tool call the cell admitted, in admission order, with the host
/// record of its outcome once that committed.
#[derive(Clone, Debug)]
pub(super) struct LedgerCall {
    /// The effect the cell performed, as its source names it.
    pub operation: OperationUse,
    pub control: bool,
    /// Where the cell performed it.
    pub effect: EffectId,
    pub state: LedgerCallState,
}

/// The call identity belongs to its pending admission or completed record.
#[derive(Clone, Debug)]
pub(super) enum LedgerCallState {
    Pending {
        call_id: lash_core::ToolCallId,
    },
    Completed {
        record: Box<lash_core::ToolCallRecord>,
    },
}

impl LedgerCall {
    fn call_id(&self) -> &lash_core::ToolCallId {
        match &self.state {
            LedgerCallState::Pending { call_id } => call_id,
            LedgerCallState::Completed { record } => &record.call_id,
        }
    }

    fn record(&self) -> Option<&lash_core::ToolCallRecord> {
        match &self.state {
            LedgerCallState::Pending { .. } => None,
            LedgerCallState::Completed { record } => Some(record),
        }
    }

    fn control(&self) -> bool {
        self.control
    }
}

/// What the host holds of a cell beside its prints.
#[derive(Clone, Debug, Default)]
pub(super) struct CellHostLedgers {
    /// Every tool call the cell's parks admitted: what it counts against
    /// `max_tool_calls`.
    pub calls: Vec<LedgerCall>,
    /// The `max_tool_calls` refusal the cell met, if it met one.
    pub tool_call_limit: Option<lash_core::ToolCallLimitExceeded>,
    /// How many reads the cell made (ADR 0144), from its saved count on.
    pub reads: u64,
}

impl CellHostLedgers {
    /// What `max_tool_calls` counts of the cell: its admitted calls and its
    /// reads. The cell's one control attempt is not counted: a cell at the
    /// limit can still end its turn.
    pub(super) fn counted(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| !call.control())
            .count()
            .saturating_add(usize::try_from(self.reads).unwrap_or(usize::MAX))
    }

    /// The records of the tool calls the cell completed, in admission
    /// order.
    pub(super) fn tool_call_records(&self) -> Vec<lash_core::ToolCallRecord> {
        self.calls
            .iter()
            .filter_map(|call| call.record().cloned())
            .collect()
    }

    /// The control the cell's control call settled as, with the call it
    /// came from: what a cell that ended on its control call is recorded
    /// as. `None` when the call failed or its body gave no control.
    pub(super) fn settled_control(
        &self,
    ) -> Option<(&lash_core::ToolCallRecord, &lash_core::TurnControl)> {
        self.calls
            .iter()
            .filter(|call| call.control())
            .find_map(|call| {
                let record = call.record()?;
                if !record.output.is_success() {
                    return None;
                }
                Some((record, record.output.as_turn_control()?))
            })
    }

    /// The cell's `Calls:` entries: every call that settled, in admission
    /// order.
    pub(super) fn executed_calls(&self) -> Vec<lash_core::ExecutedCall> {
        self.calls
            .iter()
            .filter_map(|call| {
                let record = call.record()?;
                Some(lash_core::ExecutedCall {
                    result_summary: None,
                    operation: record.tool.clone(),
                    outcome: lash_core::ExecutedCallOutcome::from(&record.output.outcome),
                    call_id: Some(record.call_id.clone()),
                })
            })
            .collect()
    }

    /// A turn-stopping cancellation, retained across parks. Closing a race
    /// loser does not stop the cell or block its next control.
    pub(super) fn call_cancelled(&self) -> bool {
        self.calls
            .iter()
            .filter_map(LedgerCall::record)
            .any(|record| record.output.cancels_turn())
    }
}

pub(super) struct CellHost<'run> {
    /// Whether the exact invocation plan binds any unrecorded reads.
    pub plans_reads: bool,
    pub ctx: RuntimeExecutionContext<'run>,
    /// The effects the cell was lowered against.
    pub execution: lash_core_store::resolutions::ExecutionId,
    /// The grants the session's deferred resolutions recorded, for a tool
    /// outside the catalog.
    /// The cell's admitted calls, by call: what their bodies run.
    pub members: Arc<CellMembers>,
    pub opener: lash_core::EffectOpener,
    pub prints: Arc<Mutex<Vec<Datum>>>,
    /// Shared with the cell's reads, which count against the same limit.
    pub ledgers: Arc<Mutex<CellHostLedgers>>,
    /// The cell's language observation, when a host asked for it.
    pub trace: Option<super::trace::CellTrace>,
    /// The effect identity each open call was performed at: where its
    /// terminal fact is published.
    pub sites: Mutex<BTreeMap<lash_core::ToolCallId, lash_vocabulary::sites::EffectId>>,
    /// What every park commits of the cell beside the ledgers and prints.
    pub envelope: CellEnvelope,
    /// The cell's host state as graph nodes, as its last park left it.
    pub graph: Mutex<HostGraph>,
    /// The cell's execution budget, whose held wall time each park saves.
    pub budget: lash_vm_client::ExecutionBudget,
}

/// A cell host's state as the graph its parks install: what an
/// acknowledged park installed, and what the park in flight staged.
#[derive(Debug, Default)]
pub(super) struct HostGraph {
    pub state: HostState,
    /// Each admitted call's Admission, by call.
    pub admissions: BTreeMap<lash_core::ToolCallId, NodeId>,
    /// Each settled call's CallOutcome, by call.
    pub outcomes: BTreeMap<lash_core::ToolCallId, NodeId>,
    /// The CellEnvelope, once a park staged it.
    pub envelope: Option<NodeId>,
    /// The nodes an acknowledged park installed beside the summary pages.
    pub installed: BTreeSet<NodeId>,
    /// What the park in flight staged, until its commit is acknowledged.
    pub staged: Option<BTreeSet<NodeId>>,
}

pub(super) fn refused(kind: &str, message: impl Into<String>) -> ErrorDatum {
    ErrorDatum {
        kind: kind.to_owned(),
        message: message.into(),
        data: Datum::Null,
    }
}

/// The error a call's or a read's own failure raises: `tool_failed`,
/// carrying the typed failure as data.
pub(super) fn tool_failed(failure: &lash_core::ToolFailure) -> ErrorDatum {
    ErrorDatum {
        kind: TOOL_FAILED.to_owned(),
        message: failure.message.clone(),
        data: datum_from_json(&failure.to_json_value().to_string()).unwrap_or(Datum::Null),
    }
}

/// The refusal of a control call, carrying its typed cause as data.
fn control_refused(effect: &OperationUse, cause: lash_core::ToolFailureCause) -> ErrorDatum {
    let message = match &cause {
        lash_core::ToolFailureCause::ControlFromSpawnedTask => format!(
            "`{effect}` ends the turn, so only the cell's main flow may call it: await it directly, not inside a concurrent task"
        ),
        lash_core::ToolFailureCause::ControlWithOutstandingTasks => format!(
            "`{effect}` ends the turn, and a task this cell started is still running or failed unobserved: await every task before calling it"
        ),
        _ => format!("`{effect}` was refused: this cell already made its one turn-ending call"),
    };
    ErrorDatum {
        kind: CONTROL_REFUSED.to_owned(),
        message,
        data: serde_json::to_string(&cause)
            .ok()
            .and_then(|json| datum_from_json(&json).ok())
            .unwrap_or(Datum::Null),
    }
}

impl CellHost<'_> {
    pub(super) fn ledgers(&self) -> CellHostLedgers {
        self.ledgers.lock_recover().clone()
    }

    /// The tool call `request` makes under `call`, frozen from the
    /// guest's own arguments, or the error its `perform` raises.
    async fn operation(
        &self,
        use_: OperationUse,
    ) -> Result<
        Result<
            (
                lash_core::OperationRoute,
                lash_vocabulary::operations::ContractId,
                lash_core::TurnControls,
            ),
            ErrorDatum,
        >,
        ParentFault,
    > {
        match self.envelope.environment.uses.get(&use_) {
            Some(UseBinding::Fixed(operation)) => Ok(Ok((
                operation.route.clone(),
                operation.requirement.contract.clone(),
                operation.offer.controls.clone(),
            ))),
            Some(UseBinding::Deferred(_)) => {
                let record = self
                    .resolve_deferred(&self.envelope.inspection, use_)
                    .await?;
                Ok(match record.entry {
                    lash_core_store::resolutions::ResolutionEntry::Resolved {
                        definition,
                        contract,
                        route,
                        ..
                    } => Ok((
                        *route,
                        contract,
                        definition.manifest.declaration().controls.clone(),
                    )),
                    lash_core_store::resolutions::ResolutionEntry::Refused { note } => {
                        Err(refused("tool_refused", note))
                    }
                    lash_core_store::resolutions::ResolutionEntry::Unavailable { note } => {
                        Err(refused(
                            "tool_unavailable",
                            note.unwrap_or_else(|| "the operation is unavailable".into()),
                        ))
                    }
                })
            }
            None => Ok(Err(refused(
                UNKNOWN_EFFECT,
                format!("the execution has no use {use_:?}"),
            ))),
        }
    }

    async fn resolve_deferred(
        &self,
        inspection: &lash_execution_contracts::process::linking::LinkInspection,
        use_: OperationUse,
    ) -> Result<lash_core_store::resolutions::ResolutionRecord, ParentFault> {
        let result = crate::deferred::resolve_use(
            &self.ctx,
            &self.execution,
            &self.envelope.environment,
            inspection,
            use_,
        )
        .await;
        let record = match result {
            Ok(lash_core::facade_support::CommittedUse::Ready(record)) => record,
            Ok(lash_core::facade_support::CommittedUse::Parked { unmet, .. }) => {
                let code = if unmet.codecs.is_empty() {
                    lash_core::RuntimeErrorCode::LashVmCellBindingDrift
                } else {
                    lash_core::RuntimeErrorCode::ExecutionBindingUnreadable
                };
                let message = format!("resolved target requires {unmet}");
                self.ctx.record_nested_runtime_effect_error(
                    lash_core::RuntimeEffectControllerError::new(code, message.clone()),
                );
                return Err(ParentFault(message));
            }
            Err(error) => {
                let fault = ParentFault(error.to_string());
                self.ctx.record_nested_effect_error(
                    lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                        error.to_string(),
                    ),
                );
                return Err(fault);
            }
        };
        if let Some(execution) = self.ctx.linked_execution() {
            execution
                .resolutions
                .lock_recover()
                .insert(use_, record.entry.clone());
        }
        Ok(record)
    }

    /// A callable handed to a declared start first uses its statically
    /// reachable deferred Imports before the start's host policy captures it.
    async fn discover_callable_uses(&self, args: &[Datum]) -> Result<(), ParentFault> {
        let mut pending: Vec<_> = args.iter().rev().collect();
        while let Some(value) = pending.pop() {
            match value {
                Datum::FunctionValue(function) => {
                    let input = lash_vm_runtime::KernelProcessInput {
                        code: lash_vm_runtime::process::InlineRequest::Callable {
                            link: function.callable.link,
                            function: function.clone(),
                            origin: lash_vocabulary::package::CallableOrigin {
                                publications: self
                                    .envelope
                                    .environment
                                    .publications
                                    .iter()
                                    .map(lash_vocabulary::package::PublicationRecord::key)
                                    .collect(),
                                authored: self.envelope.environment.authored.clone(),
                            },
                        },
                        args: Default::default(),
                    };
                    let payload = serde_json::to_value(input)
                        .map_err(|error| ParentFault(error.to_string()))?;
                    let inspection = self
                        .ctx
                        .definition_engines()
                        .inspect_code(lash_vm_runtime::LASH_VM_ENGINE_KIND, &payload)
                        .await
                        .map_err(|error| ParentFault(error.to_string()))?;
                    for (use_, need) in &inspection.operations {
                        if matches!(
                            need,
                            lash_execution_contracts::process::linking::OperationNeed::Deferred { .. }
                        ) {
                            self.resolve_deferred(&inspection, *use_).await?;
                        }
                    }
                }
                Datum::Tuple(values) | Datum::List(values) | Datum::Set(values) => {
                    pending.extend(values.iter().rev())
                }
                Datum::Map(values) => {
                    pending.extend(values.iter().rev().flat_map(|(key, value)| [value, key]))
                }
                Datum::Record(values) => {
                    pending.extend(values.iter().rev().map(|(_, value)| value))
                }
                Datum::Error(error) => pending.push(&error.data),
                _ => {}
            }
        }
        Ok(())
    }

    fn tool_call(
        &self,
        request: &EffectRequest,
        call: lash_core::ToolCallId,
        route: &lash_core::OperationRoute,
        contract: lash_vocabulary::operations::ContractId,
        grant: lash_core::ToolExecutionGrant,
    ) -> Result<(ToolInvocation, CellCall, FrozenRequest), ErrorDatum> {
        let projection = if matches!(
            self.envelope.environment.uses.get(&request.use_),
            Some(UseBinding::Deferred(_))
        ) {
            CellArgs::Deferred
        } else {
            CellArgs::Direct
        };
        let args = projection
            .args(&request.args)
            .map_err(|refusal| refusal.error(&request.use_))?;
        let invocation = ToolInvocation::new(call, route.tool.clone(), args)
            .with_issuing_language_node_id(request.identity.position().to_string())
            .with_execution_grant(grant);
        let cell = CellCall::of(&invocation, contract);
        let frozen = FrozenRequest::tool(cell.clone(), request.args.clone(), &projection)
            .map_err(|refusal| refused(TOOL_ARGUMENTS, refusal.to_string()))?;
        Ok((invocation, cell, frozen))
    }
}

/// Why a cell's arguments make no tool call.
pub(super) enum ArgsRefusal {
    NotJson(String),
    Arity(usize),
}

impl ArgsRefusal {
    /// The error a `perform` of `effect` with these arguments raises.
    pub(super) fn error(self, effect: &OperationUse) -> ErrorDatum {
        refused(
            TOOL_ARGUMENTS,
            match self {
                Self::NotJson(problem) => {
                    format!("`{effect}` takes JSON arguments, and these are not JSON: {problem}")
                }
                Self::Arity(count) => format!(
                    "`{effect}` takes one input record and is performed with {count} arguments"
                ),
            },
        )
    }
}

/// How a cell's arguments become a tool call's JSON: the one input record
/// as it left the cell, its whole numbers below 2^53 integers in either
/// dialect (`Float::json_number`) and each function value in its transport
/// form. It reads data only: a frozen request holds the arguments that left
/// the cell under this projection's identity, never the JSON it makes
/// (FIG-5887 §3), and reads back the same on any later build.
pub(super) enum CellArgs {
    Direct,
    Deferred,
}

/// The projection identity of a call's arguments.
pub(super) const CALL_ARGS: &str = "lash-cell-args/v1";
const DEFERRED_CALL_ARGS: &str = "lash-cell-deferred-args/v1";

impl CellArgs {
    /// The projection named `id`.
    pub(super) fn named(id: &str) -> Result<Self, String> {
        match id {
            CALL_ARGS => Ok(Self::Direct),
            DEFERRED_CALL_ARGS => Ok(Self::Deferred),
            other => Err(format!("no argument projection `{other}`")),
        }
    }

    pub(super) fn args(&self, args: &[Datum]) -> Result<serde_json::Value, ArgsRefusal> {
        let args = lash_vm_runtime::tool_arguments(args, matches!(self, Self::Deferred))
            .map_err(ArgsRefusal::NotJson)?;
        lash_vm_runtime::tool_input(
            args,
            |input| {
                datum_to_json(input)
                    .map_err(|error| error.to_string())
                    .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
                    .map_err(ArgsRefusal::NotJson)
            },
            ArgsRefusal::Arity,
        )
    }
}

impl lash_vm_broker::kernel::graph::ArgumentProjection for CellArgs {
    fn id(&self) -> &str {
        match self {
            Self::Direct => CALL_ARGS,
            Self::Deferred => DEFERRED_CALL_ARGS,
        }
    }

    fn project(&self, args: &[Datum]) -> Result<serde_json::Value, String> {
        self.args(args).map_err(|refusal| match refusal {
            ArgsRefusal::NotJson(problem) => problem,
            ArgsRefusal::Arity(count) => format!("{count} arguments, not one input record"),
        })
    }
}

#[async_trait::async_trait]
impl KernelEffects for CellHost<'_> {
    async fn admit(
        &self,
        request: &EffectRequest,
        call: lash_core::ToolCallId,
        outstanding: u64,
        live: u64,
    ) -> Result<Result<MemberDraft, ErrorDatum>, ParentFault> {
        let (route, contract, controls) = match self.operation(request.use_).await? {
            Ok(operation) => operation,
            Err(error) => return Ok(Err(error)),
        };
        let grant = match self.ctx.operation_grant(&route, &contract) {
            Ok(grant) => grant,
            Err(failure) => return Ok(Err(tool_failed(&failure))),
        };
        if grant
            .definition()
            .manifest
            .declaration()
            .intents
            .contains(&lash_vocabulary::ToolIntentKind::StartProcess)
        {
            self.discover_callable_uses(&request.args).await?;
        }
        let (invocation, cell, frozen) =
            match self.tool_call(request, call, &route, contract, grant) {
                Ok(called) => called,
                Err(refusal) => return Ok(Err(refusal)),
            };
        // A call to a tool that declares a turn control ends the turn when
        // it settles, so it is admitted only where the run can end on it:
        // from `main`, with no task outstanding, and once per cell. The
        // refusal comes before the body runs.
        let control = !controls.is_empty();
        // A turn-stopping cancellation ends the cell whatever its program
        // made of the error: nothing after it ends the turn in its place.
        if control && (self.ledgers.lock_recover().call_cancelled() || self.ctx.is_cancelled()) {
            return Ok(Err(refused(
                EFFECT_CANCELLED,
                format!("`{}` was not called: the turn was cancelled", request.use_),
            )));
        }
        if control {
            let cause =
                if !lash_vocabulary::sites::ObservationReader::is_root_task(&request.identity) {
                    Some(lash_core::ToolFailureCause::ControlFromSpawnedTask)
                } else if outstanding > 0 || live > 0 {
                    Some(lash_core::ToolFailureCause::ControlWithOutstandingTasks)
                } else if self
                    .ledgers
                    .lock_recover()
                    .calls
                    .iter()
                    .any(|call| call.control())
                {
                    Some(lash_core::ToolFailureCause::ControlAttemptSpent)
                } else {
                    None
                };
            if let Some(cause) = cause {
                return Ok(Err(control_refused(&request.use_, cause)));
            }
        }
        {
            let mut ledgers = self.ledgers.lock_recover();
            let counted = ledgers.counted();
            if !control && counted.saturating_add(1) > self.ctx.max_tool_calls().get() {
                let exceeded = lash_core::ToolCallLimitExceeded {
                    scope: lash_core::ToolCallLimitScope::Cell,
                    limit: self.ctx.max_tool_calls(),
                    counted,
                    requested: 1,
                };
                self.ctx.record_tool_call_limit_refusal(exceeded);
                ledgers.tool_call_limit.get_or_insert(exceeded);
                return Ok(Err(refused(TOOL_CALL_LIMIT, exceeded.to_string())));
            }
            ledgers.calls.push(LedgerCall {
                control,
                operation: request.use_,
                effect: request.identity.clone(),
                state: LedgerCallState::Pending {
                    call_id: invocation.id.clone(),
                },
            });
        }
        let now_ms = self
            .ctx
            .actor_context()
            .durable_now()
            .await
            .map_err(|error| ParentFault(error.to_string()))?;
        let now_ms = u64::try_from(now_ms.0).unwrap_or(0);
        if let Some(trace) = &self.trace {
            trace.origin(&request.identity, &request.origin);
            trace.call_started(&request.identity, &invocation.id);
            self.sites
                .lock_recover()
                .insert(invocation.id.clone(), request.identity.clone());
        }
        let member = CellMember::Tool(cell);
        // A node without the process engine the call's manifest names takes
        // up none of it: the cell stops unrecorded, and its turn waits for a
        // node that registers the engine.
        if let Err(error) = self.members.require_capable(&member) {
            let fault = ParentFault(error.to_string());
            self.ctx.record_nested_runtime_effect_error(error);
            return Err(fault);
        }
        let pin = self.members.pin(&member, now_ms);
        let draft = MemberDraft::pinned(member.id().clone(), frozen, &self.opener, pin)?;
        // The cell's admission retains the call's trace scope, which every
        // attempt and every owner traces it under.
        let trace = self.members.trace_scope(&member, now_ms);
        self.members.register(member);
        Ok(Ok(MemberDraft {
            draft: draft.draft.with_trace(trace),
            ..draft
        }))
    }

    /// The outcome of the tool call `effect` was admitted as: its value, or
    /// the error its `perform` raises. A function of the call's committed
    /// record: the ledger it fills is keyed by the call, so reading a
    /// record again changes nothing.
    fn outcome(
        &self,
        effect: &AdmittedEffect,
        output: &lash_core_store::tool_run::SettledOutput,
    ) -> Option<Outcome> {
        // `None` while the record is not final.
        if matches!(output, lash_core_store::tool_run::SettledOutput::Waiting(_)) {
            return None;
        }
        let Some(reply) = self.members.reply_to(&effect.call, output) else {
            return outcome_of(output);
        };
        if let Some(trace) = &self.trace
            && let Some(at) = self.sites.lock_recover().get(&effect.call)
        {
            trace.call_ended(at, &effect.call, &reply.output.outcome);
        }
        let mut outcome = match &reply.output.outcome {
            lash_core::ToolCallOutcome::Success(_) => {
                let value = reply.output.value_for_projection().to_string();
                match datum_from_json(&value) {
                    Ok(value) => Outcome::Completed(value),
                    Err(invalid) => Outcome::Failed(refused(
                        lash_vm_broker::kernel::EFFECT_RESULT,
                        invalid.to_string(),
                    )),
                }
            }
            lash_core::ToolCallOutcome::Failure(failure) => Outcome::Failed(tool_failed(failure)),
            lash_core::ToolCallOutcome::Cancelled(cancelled) => {
                Outcome::Failed(refused(EFFECT_CANCELLED, cancelled.message.clone()))
            }
        };
        if matches!(
            output,
            lash_core_store::tool_run::SettledOutput::Interrupted
        ) && let Outcome::Failed(error) = &mut outcome
        {
            let ledgers = self.ledgers.lock_recover();
            if let Some(call) = ledgers
                .calls
                .iter()
                .find(|call| call.call_id() == &effect.call)
                && let Some(need) = self.envelope.inspection.operations.get(&call.operation)
            {
                let name = match need {
                    lash_execution_contracts::process::linking::OperationNeed::Fixed {
                        name,
                        ..
                    }
                    | lash_execution_contracts::process::linking::OperationNeed::Deferred {
                        name,
                        ..
                    } => name.to_string(),
                };
                let identity = &call.effect;
                error.message = format!(
                    "effect `{name}` at {identity} started and never settled; outside work may already have happened, so check outside state before calling again",
                );
                error.data = Datum::Record(vec![
                    ("effect".to_owned(), Datum::Text(name)),
                    (
                        "position".to_owned(),
                        Datum::Text(identity.position().to_string()),
                    ),
                    ("identity".to_owned(), Datum::Text(identity.to_string())),
                    ("cause".to_owned(), error.data.clone()),
                ]);
            }
        }
        if let Some(record) = reply.record {
            let mut ledgers = self.ledgers.lock_recover();
            if let Some(call) = ledgers
                .calls
                .iter_mut()
                .find(|call| call.call_id() == &effect.call)
                && call.record().is_none()
            {
                call.state = LedgerCallState::Completed {
                    record: Box::new(record),
                };
            }
        }
        Some(outcome)
    }

    fn stop_requested(&self) -> bool {
        self.ledgers
            .lock_recover()
            .calls
            .iter()
            .filter_map(LedgerCall::record)
            .any(|call| call.output.tool_panic_stop().is_some())
    }

    fn resumed(&self, executions: &[(AdmittedEffect, FrozenRequest)]) -> Result<(), ParentFault> {
        for (_, request) in executions {
            let FrozenRequest::Tool { call, .. } = request else {
                continue;
            };
            let member = CellMember::Tool(call.clone());
            // An open call this node cannot run is not taken up: the cell
            // stops unrecorded, and its turn waits for a node that can.
            if let Err(error) = self.members.require_capable(&member) {
                let fault = ParentFault(error.to_string());
                self.ctx.record_nested_runtime_effect_error(error);
                return Err(fault);
            }
            self.members.register(member);
        }
        Ok(())
    }

    fn stage_host(
        &self,
        staging: &mut GraphStaging,
        reads: &mut BTreeSet<NodeId>,
        admitted: &[AdmittedCall],
    ) -> Result<HostParts, ParentFault> {
        let fault =
            |error: lash_core_store::checkpoint_graph::GraphRefusal| ParentFault(error.to_string());
        let ledgers = self.ledgers.lock_recover();
        let prints = self.prints.lock_recover();
        let mut graph = self.graph.lock_recover();
        // A park whose commit was not acknowledged staged nothing the store
        // holds: this one stages it again.
        if graph.staged.take().is_some() {
            graph.state.unstaged();
        }
        let mut staged = BTreeSet::new();
        for call in admitted {
            graph.admissions.insert(call.call.clone(), call.admission);
        }
        for call in ledgers
            .calls
            .iter()
            .skip(usize::try_from(graph.state.calls.len()).unwrap_or(usize::MAX))
        {
            let Some(admission) = graph.admissions.get(call.call_id()).copied() else {
                break;
            };
            let control = call.control();
            graph.state.admit(
                call.effect.clone(),
                call.operation,
                call.call_id().clone(),
                admission,
                control,
            );
        }
        for call in &ledgers.calls {
            let Some(record) = call.record() else {
                continue;
            };
            if graph.outcomes.contains_key(&record.call_id) {
                continue;
            }
            let Some(admission) = graph.admissions.get(&record.call_id).copied() else {
                continue;
            };
            let outcome = call_outcome(record, admission)
                .stage(staging)
                .map_err(fault)?;
            staged.insert(outcome);
            graph.outcomes.insert(record.call_id.clone(), outcome);
            graph.state.settle(&record.call_id, outcome, &record.output);
        }
        graph.state.tool_call_limit = ledgers.tool_call_limit;
        graph.state.reads = ledgers.reads;
        graph.state.held_ms = u64::try_from(self.budget.held().as_millis()).unwrap_or(u64::MAX);
        drop(ledgers);
        for value in prints
            .iter()
            .skip(usize::try_from(graph.state.prints.len()).unwrap_or(usize::MAX))
        {
            graph.state.prints.push(PrintEvent(value.clone()));
        }
        drop(prints);
        for process in self
            .ctx
            .started_process_ids()
            .into_iter()
            .skip(usize::try_from(graph.state.processes.len()).unwrap_or(usize::MAX))
        {
            graph.state.processes.push(ProcessEvent(process));
        }
        let envelope = match graph.envelope {
            Some(envelope) if graph.installed.contains(&envelope) => {
                reads.insert(envelope);
                envelope
            }
            _ => {
                let envelope = self.envelope.stage(staging).map_err(fault)?;
                staged.insert(envelope);
                graph.envelope = Some(envelope);
                envelope
            }
        };
        let host = graph.state.stage(staging, reads).map_err(fault)?;
        graph.staged = Some(staged);
        let mut links = self.envelope.environment.requirements.clone();
        if let Some(execution) = self.ctx.linked_execution() {
            for resolution in execution.resolutions.lock_recover().values() {
                if let Some(requirements) = resolution.requirements() {
                    links.extend(requirements);
                }
            }
        }
        let owed = links
            .callbacks
            .iter()
            .map(|requirement| requirement.callback.clone())
            .collect();
        Ok(HostParts {
            host: Some(host),
            envelope: Some(envelope),
            owed,
            links,
        })
    }

    fn host_committed(&self) {
        let mut graph = self.graph.lock_recover();
        if let Some(staged) = graph.staged.take() {
            graph.installed.extend(staged);
        }
    }

    fn plans_reads(&self) -> bool {
        self.plans_reads
    }

    fn held_response(&self) -> Option<&lash_core::HeldResponse> {
        self.ctx.held_response()
    }
}

impl Projections for CellHost<'_> {
    fn project(&self, projection: &str, args: &[Datum]) -> Result<serde_json::Value, String> {
        DataProjections.project(projection, args)
    }
}

/// Each frozen tool request read back through the data-only projection it
/// was frozen under.
pub(super) struct DataProjections;

impl Projections for DataProjections {
    fn project(&self, projection: &str, args: &[Datum]) -> Result<serde_json::Value, String> {
        use lash_vm_broker::kernel::graph::ArgumentProjection as _;
        CellArgs::named(projection)?.project(args)
    }
}

/// The CallOutcome `record`, a settled call's host record under
/// `admission`, is installed as: its output and its presentation, once.
/// The host answers from the call's final record, not the attempt that
/// made it, so no attempt is named.
pub(super) fn call_outcome(
    record: &lash_core::ToolCallRecord,
    admission: NodeId,
) -> lash_vm_broker::round_graph::CallOutcome {
    let model_return =
        lash_protocol_types::ModelToolReturn::from_output(record.tool.clone(), &record.output);
    lash_vm_broker::round_graph::CallOutcome {
        call_id: record.call_id.clone(),
        admission: Some(admission),
        args: record.args.clone(),
        attempt: 0,
        tool_name: record.tool.clone(),
        output: record.output.clone(),
        provider_call_id: record.provider_call_id.clone(),
        replay: None,
        intent_outcomes: Vec::new(),
        presentation: lash_protocol_types::tool_output::graph::PresentationRecipe::of(
            &model_return,
            &record.output,
        ),
    }
}
