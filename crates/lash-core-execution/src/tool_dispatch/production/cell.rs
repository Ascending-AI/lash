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
//! ([`CellMembers::propose_trace`]), and every attempt traces the call
//! under it: the call is admitted once, not once per attempt. The
//! admission owes its calls' admission exports until an owner records that
//! it exported them (`round.traced`): the cell's owner exports them before
//! any of their bodies runs, selecting the candidates it proposed or
//! reconciling the exports of another owner's, and then records so. A node
//! that takes the cell over after that record exports none of its calls'
//! admissions again (FIG-5395, FIG-5457).

use super::round::{
    catalog_policies, completed_answer, discharge_member, member_body, member_pin,
    present_resolved, resolved_member,
};
use super::*;
use crate::runtime::actor::round::{
    AdmittedExecution, Discharge, Material, MemberBodies, MemberBody, MemberPin, PolicyView,
    Presented, SettledOutput,
};
use crate::runtime::actor::waits::Resolution;
use crate::session::tool_execution::{ToolInvocation, ToolInvocationReply};
use crate::tool_run::CompletionSource;

/// One tool call a cell makes, as its admitted member's request carries
/// it: everything its body is built from again, on any owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellCall {
    /// The call's lash-minted identity.
    pub id: crate::ToolCallId,
    /// The tool it calls.
    pub tool_id: crate::ToolId,
    /// Its arguments.
    pub args: serde_json::Value,
    /// The grant a deferred resolution pinned, for a tool outside the
    /// catalog.
    pub execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    /// The binding the cell recorded for a tool that has since drifted
    /// (FIG-3587).
    pub recorded_binding: Option<Box<crate::ToolDefinition>>,
    /// The language node that issued it, for traces.
    pub issuing_language_node_id: Option<String>,
}

impl CellCall {
    /// The call `invocation` makes.
    #[must_use]
    pub fn of(invocation: &ToolInvocation) -> Self {
        Self {
            id: invocation.id.clone(),
            tool_id: invocation.tool_id.clone(),
            args: invocation.args.clone(),
            execution_grant: invocation.execution_grant.clone(),
            recorded_binding: invocation.recorded_binding.clone(),
            issuing_language_node_id: invocation.issuing_language_node_id.clone(),
        }
    }

    /// The invocation that runs it.
    #[must_use]
    pub fn invocation(&self) -> ToolInvocation {
        let mut invocation =
            ToolInvocation::new(self.id.clone(), self.tool_id.clone(), self.args.clone());
        invocation.execution_grant = self.execution_grant.clone();
        invocation.recorded_binding = self.recorded_binding.clone();
        invocation.issuing_language_node_id = self.issuing_language_node_id.clone();
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

    /// The request a cell's member carries.
    ///
    /// # Errors
    ///
    /// The call does not encode.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    /// The call a cell's member request carries.
    ///
    /// # Errors
    ///
    /// The request is not a cell's call.
    pub fn decode(request: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(request).map_err(|error| error.to_string())
    }
}

/// One admitted call of a cell, as its member's request carries it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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

    /// The request a cell's member carries.
    ///
    /// # Errors
    ///
    /// The call does not encode.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    /// The call a cell's member request carries.
    ///
    /// # Errors
    ///
    /// The request is not a cell's call.
    pub fn decode(request: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(request).map_err(|error| error.to_string())
    }
}

/// The tools a cell's calls run: the catalog of the cell's execution
/// context, under the cell's run.
pub(crate) struct CellTools {
    context: RuntimeExecutionContext<'static>,
    owner: crate::EffectOpener,
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
        let definition = ProductionToolHandlers::new(self.context.clone(), None)
            .leaf_definition(&call.invocation());
        member_pin(
            &self.context,
            definition.as_ref().map(|definition| &definition.manifest),
            definition.as_ref().map_or_else(
                || call.tool_id.clone(),
                |definition| definition.manifest.id.clone(),
            ),
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
            execution.draft().trace().cloned(),
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
        let mut pending = call.pending();
        if let Some(definition) = ProductionToolHandlers::new(self.context.clone(), None)
            .leaf_definition(&call.invocation())
        {
            pending.tool_name = definition.manifest.name.clone();
        }
        present_resolved(
            &self.context,
            &self.owner,
            &pending,
            execution.draft().tool(),
            output,
        )
        .await
    }
}

/// What `call` is answered with: a pure function of its committed
/// `output`.
fn reply(call: &crate::sansio::PendingToolCall, output: &SettledOutput) -> ToolInvocationReply {
    let completed = completed_answer(call, output);
    let record = ToolCallRecord {
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
    /// The trace candidates of the tool calls this owner proposed, until
    /// their admission's exports are owed ([`CellMembers::export_trace_admissions`]).
    candidates: Mutex<BTreeMap<crate::ToolCallId, Box<dyn lash_trace::TraceAdmissionCandidate>>>,
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
            tools: CellTools { context, owner },
            calls: Mutex::default(),
            candidates: Mutex::default(),
        })
    }

    /// Propose the trace admission of `member`, a tool call, requested at
    /// `now_ms`: the scope the cell's admission of it retains. The candidate
    /// is held until the admission, committed, has its exports discharged.
    /// `None` without tracing.
    pub fn propose_trace(
        &self,
        member: &CellMember,
        now_ms: u64,
    ) -> Option<lash_trace::DurableTraceScope> {
        let CellMember::Tool(call) = member;
        let proposal = self
            .tools
            .context
            .propose_tool_trace(&call.id, now_ms)
            .unwrap_or_else(|error| {
                self.tools.context.record_nested_effect_error(error);
                None
            })?;
        // A call proposed again replaces its earlier candidate, whose
        // admission never committed.
        if let Some(earlier) = self
            .candidates
            .lock_recover()
            .insert(call.id.clone(), proposal.candidate)
        {
            earlier.settle(lash_trace::TraceCandidateOutcome::Refused);
        }
        Some(proposal.scope)
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

    /// What the cell is answered with for `member`, from its committed
    /// `output` alone.
    #[must_use]
    pub fn reply(&self, member: &CellMember, output: &SettledOutput) -> ToolInvocationReply {
        let CellMember::Tool(call) = member;
        reply(&call.pending(), output)
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
    fn export_trace_admissions(&self, members: &[AdmittedExecution]) {
        let mut candidates = self.candidates.lock_recover();
        for execution in members {
            match candidates.remove(execution.call()) {
                Some(candidate) => candidate.settle(lash_trace::TraceCandidateOutcome::Selected),
                None => {
                    if let Some(scope) = execution.draft().trace() {
                        self.tools.context.export_tool_trace_admission(scope);
                    }
                }
            }
        }
    }

    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        match self.member(execution) {
            Some(CellMember::Tool(call)) => self.tools.body(&call, execution),
            None => unknown_member(),
        }
    }

    fn stop_grace(&self) -> std::time::Duration {
        self.tools
            .context
            .dispatch()
            .plugins
            .execution_budgets()
            .stop_grace()
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
            .plugins
            .publish_committed_state(state)
    }

    fn run_changes(&self) -> Vec<lash_durable::domain::TurnNamespaceWrite> {
        self.tools.context.dispatch().plugins.run_changes()
    }

    fn run_changes_committed(&self, written: &[lash_durable::domain::TurnNamespaceWrite]) {
        self.tools
            .context
            .dispatch()
            .plugins
            .run_changes_committed(written);
    }
}

/// A candidate still held when the cell's owner lets its members go was
/// proposed for an admission whose fate this owner cannot tell: its adapter
/// holds it until a reconcile selects it or another candidate of its call
/// wins.
impl Drop for CellMembers {
    fn drop(&mut self) {
        for candidate in std::mem::take(&mut *self.candidates.lock_recover()).into_values() {
            candidate.defer();
        }
    }
}
