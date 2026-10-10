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
//! What the host holds of the cell is plain data: the calls it admitted
//! with the records of those that settled, and its prints. Every park
//! commits it beside the machine's state ([`CellHost::host_state`]), so the
//! activation that resumes the cell carries on with all of it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::facade_support::ToolInvocation;
use lash_core::tool_dispatch::{CellCall, CellMember, CellMembers};
use lash_core::{RuntimeExecutionContext, ToolExecutionGrant};
use lash_kernel_doc::{Datum, ErrorDatum};
use lash_kernel_vm::{EffectRequest, Outcome};
use lash_sansio::sync::MutexExt;
use lash_vm_broker::kernel::{
    AdmittedEffect, EFFECT_CANCELLED, KernelEffects, datum_from_json, datum_to_json, outcome_of,
};
use lash_vm_broker::{MemberDraft, ParentFault};
use lash_vm_protocol::EncodedPayload;
use lash_vm_runtime::HostBoundary;

use super::envelope::CellEnvelope;

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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerCall {
    pub call_id: lash_core::ToolCallId,
    /// The effect the cell performed, as its source names it.
    pub operation: String,
    /// The call is to a tool that declares a turn control: the cell's one
    /// control attempt, which `max_tool_calls` does not count.
    pub control: bool,
    pub record: Option<lash_core::ToolCallRecord>,
}

/// What the host holds of a cell beside its prints.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CellHostLedgers {
    /// Every tool call the cell's parks admitted: what it counts against
    /// `max_tool_calls`.
    pub calls: Vec<LedgerCall>,
    /// The `max_tool_calls` refusal the cell met, if it met one.
    pub tool_call_limit: Option<lash_core::ToolCallLimitExceeded>,
    /// A tool call of the cell was cancelled: the cell is over, whatever
    /// its program made of the error.
    pub call_cancelled: bool,
}

impl CellHostLedgers {
    /// The records of the tool calls the cell completed, in admission
    /// order.
    pub(super) fn tool_call_records(&self) -> Vec<lash_core::ToolCallRecord> {
        self.calls
            .iter()
            .filter_map(|call| call.record.clone())
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
            .filter(|call| call.control)
            .find_map(|call| {
                let record = call.record.as_ref()?;
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
                let record = call.record.as_ref()?;
                Some(lash_core::ExecutedCall {
                    operation: call.operation.clone(),
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
}

pub(super) struct CellHost<'run> {
    pub ctx: RuntimeExecutionContext<'run>,
    /// The effects the cell was lowered against.
    pub boundary: HostBoundary,
    /// The grants the session's deferred resolutions recorded, for a tool
    /// outside the catalog.
    pub grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    /// Lash's finish tool under the turn's finish schema, when the turn
    /// states one: what a `control.finish` call is admitted under, so its
    /// input validation refuses a value the turn's required output does not
    /// admit ([`crate::control_tools::turn_finish_binding`]).
    pub finish: Option<lash_core::ToolDefinition>,
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

    /// The tool call `request` makes under `call`, or the error its
    /// `perform` raises.
    fn tool_call(
        &self,
        request: &EffectRequest,
        call: lash_core::ToolCallId,
    ) -> Result<ToolInvocation, ErrorDatum> {
        let effect = self.boundary.effect(&request.effect).ok_or_else(|| {
            refused(
                UNKNOWN_EFFECT,
                format!("this session offers no `{}`", request.effect),
            )
        })?;
        // Lash's finish tool takes the turn's answer whole, as a cell's
        // value has always left it: a whole number is an integer, a value
        // JSON has no form for keeps its kind, and no argument is `null`.
        if effect.tool.as_str() == crate::control_tools::FINISH_TOOL_ID
            && let [] | [_] = request.args.as_slice()
        {
            let answer = match request.args.first() {
                None => serde_json::Value::Null,
                Some(value) => self
                    .entries
                    .leaving(value)
                    .map(|value| crate::cell_value::datum_json(&value))
                    .map_err(|problem| {
                        refused(
                            TOOL_ARGUMENTS,
                            format!("`{}` cannot take this value: {problem}", request.effect),
                        )
                    })?,
            };
            let invocation = self.invocation(request, call, effect.tool.clone(), answer);
            // The call is admitted under the turn's finish contract, pinned
            // with it: every owner validates it the same way.
            return Ok(match &self.finish {
                Some(finish) => invocation.with_recorded_binding(finish.clone()),
                None => invocation,
            });
        }
        // A tool takes its input as one record (`HostBoundary::offer_tool`).
        let args = match request.args.as_slice() {
            [] => serde_json::Value::Object(serde_json::Map::new()),
            [input] => self
                .entries
                .leaving(input)
                .and_then(|input| datum_to_json(&input).map_err(|error| error.to_string()))
                .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
                .map(crate::projection::plain_json_for_transport)
                .map_err(|problem| {
                    refused(
                        TOOL_ARGUMENTS,
                        format!(
                            "`{}` takes JSON arguments, and these are not JSON: {problem}",
                            request.effect
                        ),
                    )
                })?,
            more => {
                return Err(refused(
                    TOOL_ARGUMENTS,
                    format!(
                        "`{}` takes one input record and is performed with {} arguments",
                        request.effect,
                        more.len()
                    ),
                ));
            }
        };
        Ok(self.invocation(request, call, effect.tool.clone(), args))
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
        let invocation = match self.tool_call(request, call) {
            Ok(invocation) => invocation,
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
        if control && (self.ledgers.lock_recover().call_cancelled || self.ctx.is_cancelled()) {
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
                .any(|call| call.control)
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
            let counted = ledgers.calls.iter().filter(|call| !call.control).count();
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
                call_id: invocation.id.clone(),
                operation: request.effect.to_string(),
                control,
                record: None,
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
        let payload = EncodedPayload(member.encode().map_err(ParentFault)?);
        let draft = MemberDraft::pinned(member.id().clone(), payload, &self.opener, pin)?;
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
        let Ok(member) = CellMember::decode(&effect.request.0) else {
            return outcome_of(output);
        };
        let reply = self.members.reply(&member, output);
        if let Some(trace) = &self.trace
            && let Some(at) = self.sites.lock_recover().get(member.id())
        {
            trace.call_ended(at, member.id(), &reply.output.outcome);
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
                self.ledgers.lock_recover().call_cancelled = true;
                Outcome::Failed(refused(EFFECT_CANCELLED, cancelled.message.clone()))
            }
        };
        if let Some(record) = reply.record {
            let mut ledgers = self.ledgers.lock_recover();
            if let Some(call) = ledgers
                .calls
                .iter_mut()
                .find(|call| &call.call_id == member.id())
                && call.record.is_none()
            {
                call.record = Some(record);
            }
        }
        Some(outcome)
    }

    fn stop_requested(&self) -> bool {
        self.ledgers
            .lock_recover()
            .calls
            .iter()
            .filter_map(|call| call.record.as_ref())
            .any(|call| call.output.tool_panic_stop().is_some())
    }

    fn host_state(&self) -> Result<Option<EncodedPayload>, ParentFault> {
        self.envelope
            .at_park(&self.ctx, self.ledgers(), &self.prints.lock_recover())
            .encode()
            .map(|bytes| Some(EncodedPayload(bytes)))
            .map_err(ParentFault)
    }
}
