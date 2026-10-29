//! A code cell's calls on the production tools (ADR 0132 §5, §8).
//!
//! Every call a cell makes is its own admitted execution: the cell's quiet
//! point admits it under the policy, limit and completion wait its tool
//! declares ([`CellMembers::pin`]), and its body runs between its `x_start`
//! and its `x_outcome`: a tool call's admission checks, attempt and
//! decision, as a round member's do. Its request is the [`CellMember`] itself, which the cell's
//! snapshot keeps for as long as the call is open, so any owner builds its
//! body again. The cell is answered from the call's committed outcome alone
//! ([`CellMembers::reply`]).
//!
//! A tool call's trace scope is retained by the cell's admission of it
//! ([`CellMembers::trace_scope`]), and every attempt traces the call's
//! start under it: the call is admitted once, not once per attempt, and its
//! span is derived from the cell's turn, so every owner names the same one.
//! Its span closes on the owner whose acknowledged commit wrote its outcome
//! ([`MemberBodies::observe`]), never inside an attempt (P1).
//!
//! When the cell's Run closes with calls still open (a race's loser), each
//! is stopped for the closing, under its own cause: a body that observes
//! its stop answers its own cancellation, attributed to the closing
//! (`CancelOrigin::RunClosing`), and a call the closing settles without its
//! body's answer (one dropped after the stop grace, one waiting out a
//! retry's backoff) is observed from its committed outcome, under the same
//! cause (FIG-5379).

use super::RunClosing;
use super::round::{
    MemberAttempts, catalog_policies, completed_answer, discharge_member, member_body, member_pin,
    present_resolved, resolved_member,
};
use super::*;
use crate::runtime::actor::round::graph::{decode_opt_tree, encode_opt_tree};
use crate::runtime::actor::round::{
    AdmittedExecution, Discharge, MemberBodies, MemberBody, MemberPin, PolicyView, Presented,
};
use crate::runtime::actor::waits::Resolution;
use crate::session::tool_execution::{ToolInvocation, ToolInvocationReply};
use crate::tool_run::CompletionSource;
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_kernel_value::tokens::TokenCodec;
use lash_kernel_value::tokens::TokenError;
use lash_kernel_value::tokens::TokenReader;
use lash_kernel_value::tokens::TokenWriter;

/// One tool call a cell makes, as its admitted member's frozen request
/// carries it: everything its body is built from again, on any owner.
#[derive(Clone, Debug, PartialEq)]
pub struct CellCall {
    /// The call's lash-minted identity.
    pub id: crate::ToolCallId,
    /// The tool it calls.
    pub tool_id: lash_vocabulary::tool_contract::ToolId,
    /// Its arguments.
    pub args: serde_json::Value,
    /// The contract the cell's code requires of the call's operation: the
    /// one its tool must serve (OP-CONTRACT-PIN).
    pub contract: lash_kernel_value::operations::ContractId,
    /// The grant a deferred resolution pinned, for a tool outside the
    /// catalog.
    pub execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    /// The language node that issued it, for traces.
    pub issuing_language_node_id: Option<String>,
}

impl CellCall {
    /// The call `invocation` makes, pinned to `contract`, the one the
    /// cell's code requires.
    #[must_use]
    pub fn of(
        invocation: &ToolInvocation,
        contract: lash_kernel_value::operations::ContractId,
    ) -> Self {
        Self {
            id: invocation.id.clone(),
            tool_id: invocation.tool_id.clone(),
            args: invocation.args.clone(),
            contract,
            execution_grant: invocation.execution_grant.clone(),
            issuing_language_node_id: invocation.issuing_language_node_id.clone(),
        }
    }

    /// The invocation that runs it.
    #[must_use]
    pub fn invocation(&self) -> ToolInvocation {
        let mut invocation =
            ToolInvocation::new(self.id.clone(), self.tool_id.clone(), self.args.clone());
        invocation.execution_grant = self.execution_grant.clone();
        invocation.issuing_language_node_id = self.issuing_language_node_id.clone();
        invocation.contract = Some(self.contract.clone());
        invocation
    }

    /// The call as the cell's records name it.
    fn pending(&self) -> crate::sansio::PendingToolCall {
        crate::sansio::PendingToolCall {
            call_id: self.id.clone(),
            provider_call_id: None,
            tool_name: self.tool_id.to_string(),
            args: self.args.clone(),
            replay: None,
        }
    }
}

impl CellCall {
    /// Writes what the call is dispatched as beside its arguments and its
    /// identity: its tool, pinned contract, grant and issuing language
    /// node. A frozen request holds this once, with the guest's own
    /// arguments, never their tool JSON (FIG-5887 §3).
    pub fn encode_dispatch(&self, out: &mut TokenWriter) {
        out.seq(4);
        out.text(self.tool_id.as_str());
        self.contract.encode(out);
        encode_opt_tree(self.execution_grant.as_deref(), out);
        self.issuing_language_node_id.encode(out);
    }

    /// Reads what [`encode_dispatch`](Self::encode_dispatch) wrote, as the
    /// call `id` with `args`, its arguments' tool projection.
    ///
    /// # Errors
    ///
    /// Any other spelling.
    pub fn decode_dispatch(
        input: &mut TokenReader<'_>,
        id: crate::ToolCallId,
        args: serde_json::Value,
    ) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            id,
            tool_id: lash_vocabulary::tool_contract::ToolId::new(input.text()?),
            args,
            contract: lash_kernel_value::operations::ContractId::decode(input)?,
            execution_grant: decode_opt_tree(input)?.map(Box::new),
            issuing_language_node_id: Option::decode(input)?,
        })
    }
}

/// One admitted call of a cell, as its member's request carries it.
#[derive(Clone, Debug, PartialEq)]
pub enum CellMember {
    /// A catalog tool call.
    Tool(CellCall),
}

impl CellMember {
    /// The call's identity.
    #[must_use]
    pub fn id(&self) -> &crate::ToolCallId {
        match self {
            Self::Tool(call) => &call.id,
        }
    }
}

/// The tools a cell's calls run: the catalog of the cell's execution
/// context, under the cell's run.
pub(crate) struct CellTools {
    context: RuntimeExecutionContext<'static>,
    owner: crate::EffectOpener,
    /// The closing of the cell's Run, which its calls' attempts read.
    closing: RunClosing,
}

impl CellTools {
    /// What `call` is admitted as, read from the catalog at `now_ms`: its
    /// tool's declared policy, a body limit starting now from its host-set
    /// execution bound, and for a tool that may defer the deadline its
    /// host-set park bound gives the completion wait its admission pins. A
    /// call no tool answers is admitted `Once`, and its body answers it
    /// unavailable. Runs no hook, preparation or body.
    #[must_use]
    pub(crate) fn pin(&self, call: &CellCall, now_ms: u64) -> MemberPin {
        let definition =
            ProductionToolHandlers::new(self.context.clone()).leaf_definition(&call.invocation());
        member_pin(
            &self.context,
            definition.as_ref().map(|definition| &definition.manifest),
            definition.as_ref().map_or_else(
                || call.tool_id.clone(),
                |definition| definition.manifest.id.clone(),
            ),
            call.execution_grant
                .as_deref()
                .map(|grant| grant.provider()),
            now_ms,
        )
    }

    /// The policies the catalog declares now: what a resumed call vetoes a
    /// stored repeat against.
    #[must_use]
    pub(crate) fn policies(&self) -> PolicyView {
        catalog_policies(&self.context)
    }

    /// The body of `execution`, an attempt of `call`, traced under the
    /// scope its admission retained.
    #[must_use]
    pub(crate) fn body(&self, call: &CellCall, execution: &AdmittedExecution) -> MemberBody {
        member_body(
            &self.context,
            &self.owner,
            call.pending(),
            call.invocation(),
            execution,
            &self.policies(),
            MemberAttempts {
                traced_scope: execution.draft().trace().cloned(),
                closing: Some(self.closing.clone()),
                stop: None,
            },
        )
    }

    /// The final answer of `call`, parked as `parked`, once one of its
    /// waits ended with `resolution`. Runs no body.
    #[must_use]
    pub(crate) fn resolved(
        &self,
        call: &CellCall,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        resolved_member(&self.owner, &call.pending(), parked, resolution)
    }

    /// The final answer `output` that `call`'s park resolved to as
    /// `execution`, presented as a body presents its own (ADR 0099 §6): the
    /// session's presentation steps run over it under the tool's name.
    pub(crate) async fn present(
        &self,
        call: &CellCall,
        execution: &AdmittedExecution,
        output: SettledOutput,
    ) -> SettledOutput {
        let pending = self.named(call);
        present_resolved(
            &self.context,
            &self.owner,
            &pending,
            execution.draft().tool(),
            execution.draft().binding(),
            output,
        )
        .await
    }
}

impl CellTools {
    /// `call` as its observations name it: under its tool's name.
    fn named(&self, call: &CellCall) -> crate::sansio::PendingToolCall {
        let mut pending = call.pending();
        if let Some(definition) =
            ProductionToolHandlers::new(self.context.clone()).leaf_definition(&call.invocation())
        {
            pending.tool_name = definition.manifest.tool();
        }
        pending
    }
}

/// A cell's runtime-settled cancellation is its Run's Closing. A stopped
/// turn consumes no cell answer: its accepted cancellation wins before
/// settlement. Keep the Closing cause on a cold read too, when this
/// activation never closed the Run itself.
fn cell_answer(
    call: &crate::sansio::PendingToolCall,
    output: &SettledOutput,
) -> crate::runtime::actor::round::CompletedCall {
    if matches!(output, SettledOutput::Cancelled { .. }) {
        return super::round::answered(
            call,
            ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("the owning Run closed before the call ended")
                    .with_origin(crate::CancelOrigin::RunClosing),
            ),
        );
    }
    completed_answer(call, output)
}

/// What `call` is answered with: a pure function of its committed
/// `output`.
fn reply(call: &crate::sansio::PendingToolCall, output: &SettledOutput) -> ToolInvocationReply {
    let completed = cell_answer(call, output);
    let record = ToolCallRecord {
        execution: Default::default(),
        read: None,
        call_id: completed.call_id.clone(),
        provider_call_id: completed.provider_call_id.clone(),
        tool: completed.tool_name.clone(),
        args: completed.args.clone(),
        output: completed.output.clone(),
    };
    let mut answer = ToolInvocationReply::from_output(record.output.clone()).with_record(record);
    answer.completed = Some(Box::new(completed));
    answer
}

/// The member bodies of one cell: its catalog tools, by the call each
/// admitted member names. A call is known once the
/// cell admitted it, or once a resumed cell's snapshot names it open.
pub struct CellMembers {
    tools: CellTools,
    calls: Mutex<BTreeMap<crate::ToolCallId, CellMember>>,
}

impl CellMembers {
    /// The members of the cell `owner` runs over `context`: its tool calls
    /// on `context`'s catalog.
    ///
    /// # Errors
    ///
    /// A context whose dispatch borrows its caller's frame, which no
    /// member's body may outlive.
    pub fn new(
        context: &RuntimeExecutionContext<'_>,
        owner: crate::EffectOpener,
    ) -> Result<Self, crate::RuntimeEffectControllerError> {
        let context = context.to_static().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeToolRunShape,
                "a cell's calls need a context that owns its dispatch",
            )
        })?;
        Ok(Self {
            tools: CellTools {
                context,
                owner,
                closing: RunClosing::default(),
            },
            calls: Mutex::default(),
        })
    }

    /// The trace scope the cell's admission of `member`, a tool call
    /// requested at `now_ms`, retains. `None` without tracing.
    pub fn trace_scope(
        &self,
        member: &CellMember,
        now_ms: u64,
    ) -> Option<lash_trace::DurableTraceScope> {
        let CellMember::Tool(call) = member;
        self.tools
            .context
            .tool_trace_scope(&call.id, now_ms)
            .unwrap_or_else(|error| {
                self.tools.context.record_nested_effect_error(error);
                None
            })
    }

    /// Know `member`, admitted or open, so its body can be built.
    pub fn register(&self, member: CellMember) {
        self.calls
            .lock_recover()
            .insert(member.id().clone(), member);
    }

    /// What `member` is admitted as at `now_ms`: a tool call as its tool
    /// declares it ([`CellTools::pin`]).
    #[must_use]
    pub fn pin(&self, member: &CellMember, now_ms: u64) -> MemberPin {
        let CellMember::Tool(call) = member;
        self.tools.pin(call, now_ms)
    }

    /// The policies the catalog declares now.
    #[must_use]
    pub fn policies(&self) -> PolicyView {
        self.tools.policies()
    }

    /// Whether this node can run `member` as it is admitted: under its
    /// retained surface entry or deferred match. A missing tool parks
    /// before any admission or attempt; one served under another contract
    /// is admitted, and its body raises `operation_contract_changed`. An isolated tool's call runs only in the process engine its manifest names, and a manifest a
    /// replayed cell or a grant recorded may name one this node does not
    /// register. Runs no hook, preparation or body.
    ///
    /// # Errors
    ///
    /// [`RuntimeErrorCode::LashVmCellBindingDrift`](crate::RuntimeErrorCode::LashVmCellBindingDrift):
    /// the recorded tool is missing on this node.
    /// [`RuntimeErrorCode::IsolationEngineUnavailable`](crate::RuntimeErrorCode::IsolationEngineUnavailable):
    /// the node takes up none of the cell, which waits for a node that
    /// registers the engine.
    pub fn require_capable(
        &self,
        member: &CellMember,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let CellMember::Tool(call) = member;
        let context = &self.tools.context;
        let dispatch = context.dispatch();
        if let Some(grant) = call.execution_grant.as_deref() {
            dispatch.routes.require_deferred_grant(grant, &call.id)?;
        } else if let Some(drift) = dispatch
            .routes
            .drift(&call.tool_id, Some(&call.contract), &dispatch.config)
            .filter(crate::ToolSurfaceDrift::is_missing)
        {
            return Err(drift.refusal(&call.id));
        }
        let Some(definition) =
            ProductionToolHandlers::new(context.clone()).leaf_definition(&call.invocation())
        else {
            return Ok(());
        };
        let Some(engine) = definition.manifest.isolation_engine() else {
            return Ok(());
        };
        if context.dispatch().process_engines.require(engine).is_ok() {
            return Ok(());
        }
        Err(
            lash_execution_contracts::plugin_error::PluginError::ToolRegistrationRefused {
                source: Box::new(lash_vocabulary::tool_contract::RegistrationRefused::UnregisteredIsolationEngine {
                    tool: definition.manifest.tool(),
                    engine: engine.to_owned(),
                }),
            }
            .into(),
        )
    }

    /// What the cell is answered with for `member`, from its committed
    /// `output` alone.
    #[must_use]
    pub fn reply(&self, member: &CellMember, output: &SettledOutput) -> ToolInvocationReply {
        let CellMember::Tool(call) = member;
        reply(&call.pending(), output)
    }

    /// What the cell is answered with for the call `call`, from its
    /// committed `output` alone; `None` for a call it does not know.
    #[must_use]
    pub fn reply_to(
        &self,
        call: &crate::ToolCallId,
        output: &SettledOutput,
    ) -> Option<ToolInvocationReply> {
        let member = self.calls.lock_recover().get(call).cloned()?;
        Some(self.reply(&member, output))
    }

    fn member(&self, execution: &AdmittedExecution) -> Option<CellMember> {
        self.calls.lock_recover().get(execution.call()).cloned()
    }
}

/// A member whose call the cell does not know: it reached no durable
/// answer, so it is `Interrupted`.
fn unknown_member() -> MemberBody {
    Box::new(|_| Box::pin(async { SettledOutput::Interrupted.into() }))
}

impl MemberBodies for CellMembers {
    /// Close the span of each call whose outcome this activation's
    /// acknowledged commit wrote, at the end that commit retained (P1): an
    /// attempt that ends closes nothing, and a successor that folds an
    /// outcome another owner committed closes nothing.
    fn observe(
        &self,
        round: &crate::runtime::actor::round::RoundView,
        _admitted_here: &std::collections::BTreeSet<lash_durable::domain::AdmittedId>,
        settled_here: &std::collections::BTreeSet<lash_durable::domain::AdmittedId>,
    ) {
        for member in round.members() {
            if !settled_here.contains(&round.id_of(member)) {
                continue;
            }
            let (Some(output), Some(CellMember::Tool(call))) = (
                member.outcome(),
                self.calls.lock_recover().get(member.call()).cloned(),
            ) else {
                continue;
            };
            let pending = self.tools.named(&call);
            let context = &self.tools.context;
            if let Some(scope) = member.draft().trace() {
                context.adopt_tool_call_trace(&call.id, scope.clone());
            }
            let attempts = member
                .attempts()
                .map(|(ordinal, output, delay)| {
                    crate::trace::trace_tool_attempt(
                        ordinal,
                        &super::round::member_record(&pending, output),
                        delay,
                    )
                })
                .collect::<Vec<_>>();
            let completed = cell_answer(&pending, output);
            let record = ToolCallRecord {
                execution: Default::default(),
                read: None,
                call_id: completed.call_id,
                provider_call_id: completed.provider_call_id,
                tool: completed.tool_name,
                args: completed.args,
                output: completed.output,
            };
            context.trace_tool_call_completed(&record, &attempts, member.ended_at_ms());
            context.emit_tool_call_completed_activity(call.id.as_str(), &record, None);
        }
    }

    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        match self.member(execution) {
            Some(CellMember::Tool(call)) => self.tools.body(&call, execution),
            None => unknown_member(),
        }
    }

    fn stop_grace(&self) -> std::time::Duration {
        self.tools.context.dispatch().execution_budgets.stop_grace()
    }

    fn resolved(
        &self,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        match self.member(execution) {
            Some(CellMember::Tool(call)) => self.tools.resolved(&call, parked, resolution),
            None => SettledOutput::Interrupted,
        }
    }

    fn present<'a>(
        &'a self,
        execution: &'a AdmittedExecution,
        output: SettledOutput,
    ) -> Presented<'a> {
        Box::pin(async move {
            match self.member(execution) {
                Some(CellMember::Tool(call)) => self.tools.present(&call, execution, output).await,
                // Only a tool call parks.
                _ => output,
            }
        })
    }

    fn closing(&self) {
        self.tools.closing.close();
    }

    fn discharge<'a>(
        &'a self,
        execution: &'a AdmittedExecution,
        parked: &'a Material<CompletionSource>,
        cancelled: bool,
    ) -> Discharge<'a> {
        Box::pin(discharge_member(
            &self.tools.context,
            execution.call(),
            parked,
            cancelled,
        ))
    }

    fn publish_state(
        &self,
        state: &[crate::plugin::StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.tools
            .context
            .dispatch()
            .state
            .publish_committed_state(state)
    }

    fn run_changes(&self) -> Vec<lash_durable::domain::TurnNamespaceWrite> {
        self.tools.context.dispatch().state.run_changes()
    }

    fn run_changes_committed(&self, written: &[lash_durable::domain::TurnNamespaceWrite]) {
        self.tools
            .context
            .dispatch()
            .state
            .run_changes_committed(written);
    }
}
