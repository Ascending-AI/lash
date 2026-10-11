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
//! - a `print` is an observation, kept in order.
//! - the clock, the random source and reads through projection handles are
//!   answered by the run's [`lash_vm_runtime::ParentHost`].
//!
//! What the host holds of the cell is the calls it admitted with the
//! records of those that settled, and its prints. Every park commits it
//! beside the machine's state as the cell's HostRoot and CellEnvelope
//! ([`KernelEffects::stage_host`]): each settled call's CallOutcome is
//! installed once, and a park installs only the summary pages it changed,
//! so the activation that resumes the cell carries on with all of it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core::facade_support::ToolInvocation;
use lash_core::tool_dispatch::{CellCall, CellMember, CellMembers};
use lash_core::{RuntimeExecutionContext, ToolExecutionGrant};
use lash_core_store::checkpoint_graph::{GraphStaging, NodeId};
use lash_kernel_doc::{Datum, EffectIdentity, EffectName, ErrorDatum};
use lash_kernel_vm::{EffectRequest, Outcome};
use lash_sansio::sync::MutexExt;
use lash_vm_broker::kernel::graph::{FrozenRequest, Projections};
use lash_vm_broker::kernel::{
    AdmittedCall, AdmittedEffect, EFFECT_CANCELLED, HostParts, KernelEffects, datum_from_json,
    datum_to_json, outcome_of,
};
use lash_vm_broker::{MemberDraft, ParentFault};
use lash_vm_runtime::HostBoundary;

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
    pub operation: EffectName,
    /// Where the cell performed it.
    pub effect: EffectIdentity,
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

    fn control(&self, boundary: &HostBoundary) -> bool {
        boundary
            .effect(&self.operation)
            .is_some_and(|effect| !effect.controls.is_empty())
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
}

impl CellHostLedgers {
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
        boundary: &HostBoundary,
    ) -> Option<(&lash_core::ToolCallRecord, &lash_core::TurnControl)> {
        self.calls
            .iter()
            .filter(|call| call.control(boundary))
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
                    operation: call.operation.to_string(),
                    outcome: if record.output.is_success() {
                        lash_core::ExecutedCallOutcome::Ok
                    } else {
                        lash_core::ExecutedCallOutcome::Err
                    },
                    call_id: Some(record.call_id.clone()),
                })
            })
            .collect()
    }

    /// Cancellation is the completed call's outcome, retained across parks.
    pub(super) fn call_cancelled(&self) -> bool {
        self.calls
            .iter()
            .filter_map(LedgerCall::record)
            .any(|record| {
                matches!(
                    record.output.outcome,
                    lash_core::ToolCallOutcome::Cancelled(_)
                )
            })
    }
}

pub(super) struct CellHost<'run> {
    pub ctx: RuntimeExecutionContext<'run>,
    /// The effects the cell was lowered against.
    pub boundary: HostBoundary,
    /// The grants the session's deferred resolutions recorded, for a tool
    /// outside the catalog.
    pub grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    /// The cell's admitted calls, by call: what their bodies run.
    pub members: Arc<CellMembers>,
    pub opener: lash_core::EffectOpener,
    pub prints: Arc<Mutex<Vec<Datum>>>,
    pub ledgers: Mutex<CellHostLedgers>,
    /// The document the cell's function references name entries of.
    pub entries: super::processes::CellEntries,
    /// The cell's language observation, when a host asked for it.
    pub trace: Option<super::trace::CellTrace>,
    /// The effect identity each open call was performed at: where its
    /// terminal fact is published.
    pub sites: Mutex<BTreeMap<lash_core::ToolCallId, lash_kernel_doc::EffectIdentity>>,
    /// What every park commits of the cell beside the ledgers and prints.
    pub envelope: CellEnvelope,
    /// The cell's host state as graph nodes, as its last park left it.
    pub graph: Mutex<HostGraph>,
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

fn refused(kind: &str, message: impl Into<String>) -> ErrorDatum {
    ErrorDatum {
        kind: kind.to_owned(),
        message: message.into(),
        data: Datum::Null,
    }
}

/// The refusal of a control call, carrying its typed cause as data.
fn control_refused(
    effect: &lash_kernel_doc::EffectName,
    cause: lash_core::ToolFailureCause,
) -> ErrorDatum {
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
    fn tool_call(
        &self,
        request: &EffectRequest,
        call: lash_core::ToolCallId,
    ) -> Result<(ToolInvocation, FrozenRequest), ErrorDatum> {
        if let Some(note) = self.envelope.unavailable_notes.get(request.effect.as_str()) {
            return Err(refused(
                UNKNOWN_EFFECT,
                format!("this session cannot offer `{}`: {note}", request.effect),
            ));
        }
        let effect = self.boundary.effect(&request.effect).ok_or_else(|| {
            refused(
                UNKNOWN_EFFECT,
                format!("this session offers no `{}`", request.effect),
            )
        })?;
        let projection = CellArgs {
            entries: &self.entries,
            finish: effect.controls.finish_declaration().is_some(),
        };
        let args = projection.args(&request.args).map_err(|refusal| {
            refused(
                TOOL_ARGUMENTS,
                match refusal {
                    ArgsRefusal::NotJson(problem) => format!(
                        "`{}` takes JSON arguments, and these are not JSON: {problem}",
                        request.effect
                    ),
                    ArgsRefusal::Arity(count) => format!(
                        "`{}` takes one input record and is performed with {count} arguments",
                        request.effect
                    ),
                },
            )
        })?;
        let invocation = self.invocation(request, call, effect.tool.clone(), args);
        let frozen =
            FrozenRequest::tool(CellCall::of(&invocation), request.args.clone(), &projection)
                .map_err(|refusal| refused(TOOL_ARGUMENTS, refusal.to_string()))?;
        Ok((invocation, frozen))
    }

    /// The invocation of `tool` under `call`, with the grant a tool outside
    /// the catalog runs under.
    fn invocation(
        &self,
        request: &EffectRequest,
        call: lash_core::ToolCallId,
        tool: lash_core::ToolId,
        args: serde_json::Value,
    ) -> ToolInvocation {
        let mut invocation = ToolInvocation::new(call, tool, args)
            .with_issuing_language_node_id(request.identity.site.to_string());
        if self
            .ctx
            .callable_tool_manifest_by_id(&invocation.tool_id)
            .is_none()
            && let Some(grant) = self.grants.get(&invocation.tool_id)
        {
            invocation = invocation.with_execution_grant(grant.clone());
        }
        invocation
    }
}

/// Why a cell's arguments make no tool call.
enum ArgsRefusal {
    NotJson(String),
    Arity(usize),
}

/// How a cell's arguments become a tool call's JSON: the guest's one input
/// record, leaving the cell as a cell's value always has. A finish value's
/// whole numbers are integers, in either dialect; any other call's
/// attachments reach the tool as the attachments they are. A frozen request
/// holds the guest's arguments under this projection's identity, never the
/// JSON it makes (FIG-5887 §3).
pub(super) struct CellArgs<'h> {
    pub entries: &'h super::processes::CellEntries,
    pub finish: bool,
}

/// The projection identity of a finish call's arguments.
pub(super) const FINISH_ARGS: &str = "lash-cell-args/finish/v1";
/// The projection identity of any other call's arguments.
pub(super) const CALL_ARGS: &str = "lash-cell-args/v1";

impl<'h> CellArgs<'h> {
    /// The projection named `id`, over `entries`.
    pub(super) fn named(
        id: &str,
        entries: &'h super::processes::CellEntries,
    ) -> Result<Self, String> {
        match id {
            FINISH_ARGS => Ok(Self {
                entries,
                finish: true,
            }),
            CALL_ARGS => Ok(Self {
                entries,
                finish: false,
            }),
            other => Err(format!("no argument projection `{other}`")),
        }
    }

    fn args(&self, args: &[Datum]) -> Result<serde_json::Value, ArgsRefusal> {
        // A tool takes its input as one record (`HostBoundary::offer_tool`).
        match args {
            [] => Ok(serde_json::Value::Object(serde_json::Map::new())),
            [input] => self
                .entries
                .leaving(input)
                .and_then(|input| datum_to_json(&input).map_err(|error| error.to_string()))
                .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
                .map(crate::projection::plain_json_for_transport)
                .map(|args| {
                    if self.finish {
                        crate::cell_value::whole_numbers_as_integers(args)
                    } else {
                        crate::cell_value::attachments_as_written(args)
                    }
                })
                .map_err(ArgsRefusal::NotJson),
            more => Err(ArgsRefusal::Arity(more.len())),
        }
    }
}

impl lash_vm_broker::kernel::graph::ArgumentProjection for CellArgs<'_> {
    fn id(&self) -> &str {
        if self.finish { FINISH_ARGS } else { CALL_ARGS }
    }

    fn project(&self, args: &[Datum]) -> Result<serde_json::Value, String> {
        self.args(args).map_err(|refusal| match refusal {
            ArgsRefusal::NotJson(problem) => problem,
            ArgsRefusal::Arity(count) => format!("{count} arguments, not one input record"),
        })
    }
}

/// A site as the label a trace and a tool call carry: its unit and its
/// path in the document (`K-SITE-001`).
pub(crate) fn site_label(site: &lash_kernel_doc::Site) -> String {
    let unit = match &site.unit {
        lash_kernel_doc::Unit::Main => "main".to_owned(),
        lash_kernel_doc::Unit::Function(name) => format!("fn:{name}"),
        lash_kernel_doc::Unit::Library(function) => format!("lib:{function}"),
    };
    let path = site
        .path
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".");
    format!("{unit}/{path}")
}

#[async_trait::async_trait]
impl KernelEffects for CellHost<'_> {
    async fn admit(
        &self,
        request: &EffectRequest,
        call: lash_core::ToolCallId,
        outstanding: &[lash_kernel_doc::TaskIdentity],
        live: &[lash_kernel_doc::TaskIdentity],
    ) -> Result<Result<MemberDraft, ErrorDatum>, ParentFault> {
        let (invocation, frozen) = match self.tool_call(request, call) {
            Ok(called) => called,
            Err(refusal) => return Ok(Err(refusal)),
        };
        // A call to a tool that declares a turn control ends the turn when
        // it settles, so it is admitted only where the run can end on it:
        // from `main`, with no task outstanding, and once per cell. The
        // refusal comes before the body runs.
        let control = self
            .boundary
            .effect(&request.effect)
            .is_some_and(|effect| !effect.controls.is_empty());
        // A cancelled call ends the cell whatever its program made of the
        // error, and so does the turn's own cancel: nothing after either
        // ends the turn in its place.
        if control && (self.ledgers.lock_recover().call_cancelled() || self.ctx.is_cancelled()) {
            return Ok(Err(refused(
                EFFECT_CANCELLED,
                format!(
                    "`{}` was not called: the turn was cancelled",
                    request.effect
                ),
            )));
        }
        if control {
            let cause = if request.identity.task != lash_kernel_doc::TaskIdentity::Main {
                Some(lash_core::ToolFailureCause::ControlFromSpawnedTask)
            } else if !outstanding.is_empty() || !live.is_empty() {
                Some(lash_core::ToolFailureCause::ControlWithOutstandingTasks)
            } else if self
                .ledgers
                .lock_recover()
                .calls
                .iter()
                .any(|call| call.control(&self.boundary))
            {
                Some(lash_core::ToolFailureCause::ControlAttemptSpent)
            } else {
                None
            };
            if let Some(cause) = cause {
                return Ok(Err(control_refused(&request.effect, cause)));
            }
        }
        {
            let mut ledgers = self.ledgers.lock_recover();
            // The cell's one control attempt is not a call `max_tool_calls`
            // counts: a cell at the limit can still end its turn.
            let counted = ledgers
                .calls
                .iter()
                .filter(|call| !call.control(&self.boundary))
                .count();
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
                operation: request.effect.clone(),
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
            trace.call_started(&request.identity, &invocation.id);
            self.sites
                .lock_recover()
                .insert(invocation.id.clone(), request.identity.clone());
        }
        let member = CellMember::Tool(CellCall::of(&invocation));
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
        let trace = self.members.propose_trace(&member, now_ms);
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
        output: &lash_core::SettledOutput,
    ) -> Option<Outcome> {
        // `None` while the record is not final.
        outcome_of(output)?;
        let Some(reply) = self.members.reply_to(&effect.call, output) else {
            return outcome_of(output);
        };
        if let Some(trace) = &self.trace
            && let Some(at) = self.sites.lock_recover().get(&effect.call)
        {
            trace.call_ended(at, &effect.call, &reply.output.outcome);
        }
        let outcome = match &reply.output.outcome {
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
            lash_core::ToolCallOutcome::Failure(failure) => Outcome::Failed(ErrorDatum {
                kind: TOOL_FAILED.to_owned(),
                message: failure.message.clone(),
                data: datum_from_json(&failure.to_json_value().to_string()).unwrap_or(Datum::Null),
            }),
            lash_core::ToolCallOutcome::Cancelled(cancelled) => {
                Outcome::Failed(refused(EFFECT_CANCELLED, cancelled.message.clone()))
            }
        };
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
        let ledgers = self.ledgers();
        let prints = self.prints.lock_recover().clone();
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
            let control = call.control(&self.boundary);
            graph.state.admit(
                call.effect.clone(),
                call.operation.clone(),
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
        for value in prints
            .iter()
            .skip(usize::try_from(graph.state.prints.len()).unwrap_or(usize::MAX))
        {
            graph.state.prints.push(PrintEvent(value.clone()));
        }
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
                let release = super::envelope::cell_release().map_err(ParentFault)?;
                let envelope = self.envelope.stage(staging, release).map_err(fault)?;
                staged.insert(envelope);
                graph.envelope = Some(envelope);
                envelope
            }
        };
        let host = graph.state.stage(staging, reads).map_err(fault)?;
        graph.staged = Some(staged);
        Ok(HostParts {
            host: Some(host),
            envelope: Some(envelope),
        })
    }

    fn host_committed(&self) {
        let mut graph = self.graph.lock_recover();
        if let Some(staged) = graph.staged.take() {
            graph.installed.extend(staged);
        }
    }
}

impl Projections for CellHost<'_> {
    fn project(&self, projection: &str, args: &[Datum]) -> Result<serde_json::Value, String> {
        use lash_vm_broker::kernel::graph::ArgumentProjection as _;
        CellArgs::named(projection, &self.entries)?.project(args)
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
        lash_sansio::ModelToolReturn::from_output(record.tool.clone(), &record.output);
    lash_vm_broker::round_graph::CallOutcome {
        call_id: record.call_id.clone(),
        admission: Some(admission),
        attempt: 0,
        tool_name: record.tool.clone(),
        output: record.output.clone(),
        provider_call_id: record.provider_call_id.clone(),
        replay: None,
        intent_outcomes: Vec::new(),
        presentation: lash_sansio::tool_output::graph::PresentationRecipe::of(
            &model_return,
            &record.output,
        ),
    }
}
