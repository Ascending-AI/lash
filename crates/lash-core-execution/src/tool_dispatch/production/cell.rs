//! A code cell's calls on the production tools (ADR 0132 §5, §8).
//!
//! Every call a cell makes is its own admitted execution: the cell's quiet
//! point admits it under the policy, limit and completion wait its tool
//! declares ([`CellMembers::pin`]), and its body runs between its `x_start`
//! and its `x_outcome`: a tool call's admission checks, attempt and
//! decision, as a round member's do, or a host call (a trigger command)
//! once. Its request is the [`CellMember`] itself, which the cell's
//! snapshot keeps for as long as the call is open, so any owner builds its
//! body again. The cell is answered from the call's committed outcome alone
//! ([`CellMembers::reply`]).

use std::future::Future;
use std::pin::Pin;

use super::round::{
    MemberEnd, answered, catalog_policies, completed_answer, discharge_member, member_body,
    member_output, member_pin, resolved_member,
};
use super::*;
use crate::runtime::actor::round::{
    AdmittedExecution, Discharge, Material, MemberBodies, MemberBody, MemberPin, PolicyView,
    SettledOutput,
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

/// A call a cell's host answers itself rather than through a catalog tool:
/// a trigger command. It is admitted `Once`, so a crash inside it is
/// `Interrupted`, never a second write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCall {
    /// The call's lash-minted identity.
    pub id: crate::ToolCallId,
    /// The host operation it performs.
    pub operation: String,
    /// Its payload.
    pub payload: serde_json::Value,
}

impl HostCall {
    fn pending(&self) -> crate::sansio::PendingToolCall {
        crate::sansio::PendingToolCall {
            call_id: self.id.clone(),
            provider_call_id: None,
            tool_name: self.operation.clone(),
            args: self.payload.clone(),
            replay: None,
        }
    }
}

/// One admitted call of a cell, as its member's request carries it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CellMember {
    /// A catalog tool call.
    Tool(CellCall),
    /// A call the host answers itself.
    Host(HostCall),
}

impl CellMember {
    /// The call's identity.
    #[must_use]
    pub fn id(&self) -> &crate::ToolCallId {
        match self {
            Self::Tool(call) => &call.id,
            Self::Host(call) => &call.id,
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

/// The host's own calls: what a [`HostCall`]'s body runs. Its answer is the
/// host's encoding, handed back to it by [`CellMembers::reply`].
pub trait CellHostCalls: Send + Sync {
    /// Run `call` once over the cell's `context`, to its end: a host call
    /// is one store write, never stopped halfway.
    fn run(
        &self,
        context: RuntimeExecutionContext<'static>,
        call: HostCall,
    ) -> Pin<Box<dyn Future<Output = serde_json::Value> + Send>>;
}

/// The tools a cell's calls run: the catalog of the cell's execution
/// context, under the cell's run.
pub(crate) struct CellTools {
    context: RuntimeExecutionContext<'static>,
    owner: crate::EffectOpener,
}

impl CellTools {
    /// What `call` is admitted as, read from the catalog at `now_ms`: its
    /// tool's declared policy, a limit starting now within the tool
    /// ceiling, and for a tool that may defer the completion wait its
    /// admission pins. A call no tool answers is admitted `Once`, and its
    /// body answers it unavailable. Runs no hook, preparation or body.
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

    /// The body of `execution`, an attempt of `call`.
    #[must_use]
    pub(crate) fn body(&self, call: &CellCall, execution: &AdmittedExecution) -> MemberBody {
        member_body(
            &self.context,
            &self.owner,
            call.pending(),
            call.invocation(),
            execution,
            &self.policies(),
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

/// The member bodies of one cell: its catalog tools and its host's own
/// calls, by the call each admitted member names. A call is known once the
/// cell admitted it, or once a resumed cell's snapshot names it open.
pub struct CellMembers {
    tools: CellTools,
    host: Arc<dyn CellHostCalls>,
    calls: Mutex<BTreeMap<crate::ToolCallId, CellMember>>,
}

impl CellMembers {
    /// The members of the cell `owner` runs over `context`: its tool calls
    /// on `context`'s catalog, its host calls run by `host`.
    ///
    /// # Errors
    ///
    /// A context whose dispatch borrows its caller's frame, which no
    /// member's body may outlive.
    pub fn new(
        context: &RuntimeExecutionContext<'_>,
        owner: crate::EffectOpener,
        host: Arc<dyn CellHostCalls>,
    ) -> Result<Self, crate::RuntimeEffectControllerError> {
        let context = context.to_static().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeToolRunShape,
                "a cell's calls need a context that owns its dispatch",
            )
        })?;
        Ok(Self {
            tools: CellTools { context, owner },
            host,
            calls: Mutex::default(),
        })
    }

    /// Know `member`, admitted or open, so its body can be built.
    pub fn register(&self, member: CellMember) {
        self.calls
            .lock_recover()
            .insert(member.id().clone(), member);
    }

    /// What `member` is admitted as at `now_ms`: a tool call as its tool
    /// declares it ([`CellTools::pin`]), a host call `Once` under the tool
    /// default limit.
    #[must_use]
    pub fn pin(&self, member: &CellMember, now_ms: u64) -> MemberPin {
        match member {
            CellMember::Tool(call) => self.tools.pin(call, now_ms),
            CellMember::Host(call) => member_pin(
                &self.tools.context,
                None,
                crate::ToolId::new(format!("cell-host:{}", call.operation)),
                now_ms,
            ),
        }
    }

    /// The policies the catalog declares now.
    #[must_use]
    pub fn policies(&self) -> PolicyView {
        self.tools.policies()
    }

    /// What the cell is answered with for `member`, from its committed
    /// `output` alone. A host call's answer is the success value its host
    /// encoded.
    #[must_use]
    pub fn reply(&self, member: &CellMember, output: &SettledOutput) -> ToolInvocationReply {
        match member {
            CellMember::Tool(call) => reply(&call.pending(), output),
            CellMember::Host(call) => reply(&call.pending(), output),
        }
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
    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        match self.member(execution) {
            Some(CellMember::Tool(call)) => self.tools.body(&call, execution),
            Some(CellMember::Host(call)) => {
                let host = Arc::clone(&self.host);
                let owner = self.tools.owner.clone();
                let context = self.tools.context.clone();
                Box::new(move |_| {
                    Box::pin(async move {
                        let pending = call.pending();
                        let answer = host.run(context, call).await;
                        member_output(
                            &owner,
                            MemberEnd::Final(answered(&pending, ToolCallOutput::success(answer))),
                        )
                        .into()
                    })
                })
            }
            None => unknown_member(),
        }
    }

    fn resolved(
        &self,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        match self.member(execution) {
            Some(CellMember::Tool(call)) => self.tools.resolved(&call, parked, resolution),
            // Only a tool call parks.
            _ => SettledOutput::Interrupted,
        }
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
}
