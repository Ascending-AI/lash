//! How a host process engine's steps run (ADR 0132 §10; L6, FIG-5175).
//!
//! A [`StepRequest`] names a catalog tool or one of the engine's own bodies
//! that its registration declares ([`EngineSteps`](super::EngineSteps)). A
//! `call_dynamic` call is two steps (P6): its resolution, whose committed
//! outcome is the match ([`dynamic_resolution`]), and the call of the
//! matched tool, which the commit consuming the resolution admits
//! ([`dynamic_target`]); a refused or unavailable decision answers the
//! engine typed ([`dynamic_decision`]). The
//! process actor admits it as an admitted execution (S4) under the tool's
//! declared [`ExecutionPolicy`] (an engine body's is the retry policy its
//! kind declares) and an [`ExecutionLimit`] within the tool ceiling,
//! commits that admission with the state that asked for it, and only then
//! runs the body. A step runs the admitted-execution lifecycle a round
//! member runs ([`lifecycle`](crate::runtime::actor::round::lifecycle)): a
//! `Repeatable` failure its contract repeats is retried at the run's next
//! ordinal, and a step that may park has its completion wait pinned with
//! its admission and settles from that wait's resolution. The host supplies
//! every half through [`ProcessSteps`]; there is no default.
//!
//! **Plugin state.** An engine process's plugin namespaces are the process
//! owner's: its tool steps run on the process's own plugin session, which
//! one activation of its actor holds ([`StepRuntime`]). A tool step's
//! resolutions ride its `x_outcome`, and the lifecycle publishes them into
//! that session from the committed record only, as a round member's
//! (ADR 0132 §5). A new activation builds the session again, and its
//! lifecycle publishes every committed step outcome's resolutions into it
//! from the rows before any step runs on it. A `SessionTurn` process has no
//! steps: its child turn's tools change the child session's namespaces.

use std::future::Future;
use std::sync::{Arc, Mutex};

use lash_core_store::tool_run::CompletionSource;
use lash_core_store::tool_run::StateResolution;
use lash_protocol_types::sync::MutexExt as _;
use lash_protocol_types::{ExecutionLimit, ExecutionPolicy};

use lash_core_store::tool_run::{MaterialOwner, MaterialRole};

use crate::ActorContext;
use crate::ProcessId;
use crate::RuntimeEffectControllerError;
use crate::ToolCatalog;
use crate::ToolId;
use crate::runtime::actor::round::{
    AdmittedExecution, MemberBody, MemberOutcome, RoundTools, decode_completed,
};
use crate::runtime::actor::waits::Resolution;
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::plugin_error::PluginError;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::StepRequest;
use lash_execution_contracts::waits::ParkDeadline;

/// A step's admission: what its tool's declaration pins before it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct StepAdmission {
    /// The tool its admission records: the linked tool, or an engine step's
    /// [`EngineStepKind::admitted_tool`](lash_execution_contracts::process::EngineStepKind::admitted_tool).
    pub tool: ToolId,
    /// The tool's execution policy.
    pub policy: ExecutionPolicy,
    /// The step body's limit, from the bound its host set.
    pub limit: ExecutionLimit,
    /// For a step that may park, what its park pins: the deadline of the
    /// completion wait its admission pins, separate from the body's limit,
    /// or none for a park that lasts until its process scope ends.
    pub park: Option<ParkDeadline>,
    /// The callbacks its admission binds (S7): what the process's claim
    /// obligations hold until the step settles. `None` for an engine step.
    pub binding: Option<crate::tool_run::AdmittedBinding>,
    /// The outcome a step settles with at its admission, without a body
    /// (D3): its linked tool is missing from this node's provider, or
    /// changed since the env retained its surface. It commits with the
    /// admission; no `x_start` of it runs a body.
    pub settled: Option<SettledOutput>,
}

impl StepAdmission {
    /// The admission of a step of `tool` under `policy`, `limit` and
    /// `park`, binding nothing and settled by its body.
    #[must_use]
    pub fn new(
        tool: ToolId,
        policy: ExecutionPolicy,
        limit: ExecutionLimit,
        park: Option<ParkDeadline>,
    ) -> Self {
        Self {
            tool,
            policy,
            limit,
            park,
            binding: None,
            settled: None,
        }
    }
}

/// A step refused before its admission; nothing was recorded. The process
/// ends `Failed` with it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StepRefusal {
    /// The step names a tool the process's catalog does not hold.
    #[error("process step `{step}` names `{tool}`, which the process's catalog does not hold")]
    UnknownTool {
        /// The step.
        step: String,
        /// The tool it names.
        tool: String,
    },
    /// The tool's declaration refuses the step: its input does not match,
    /// or its declared execution is above the ceiling.
    #[error("process step `{step}` is refused by its tool: {reason}")]
    Refused {
        /// The step.
        step: String,
        /// Why.
        reason: String,
    },
    /// The step's declaration could not be read now (its process's runtime
    /// did not build). Nothing is recorded, and the pass that asked is run
    /// again.
    #[error("process step `{step}` could not be admitted now: {reason}")]
    Unavailable {
        /// The step.
        step: String,
        /// Why.
        reason: String,
    },
    /// The step names an engine body the process's engine does not declare.
    #[error(transparent)]
    Engine(#[from] lash_execution_contracts::process::EngineStepRefusal),
    /// The process's composition does not build on this node (FIG-6034):
    /// the activation parks it before any step is admitted or resumed.
    #[error("the process's composition does not build here: {reason}")]
    CompositionUnavailable {
        /// Why.
        reason: String,
    },
    /// This node refuses the process's sealed env, typed: a component it
    /// names is missing, unreadable or refused to build, or the run depends
    /// on one this node declares retired. The activation parks it before
    /// any step is admitted or resumed.
    #[error("the process's env is refused here: {refusal}")]
    EnvRefused {
        /// The refusal.
        refusal: crate::ProcessEnvRefusal,
    },
}

/// The tools a process's tool steps run: the catalog they resolve against,
/// and the round tools that pin a catalog tool's policy and limit and run
/// its body through the tool dispatch a turn's round runs on, on the
/// process's own plugin session.
pub struct ProcessStepTools {
    /// The catalog of the linked tools this node serves as the env retained
    /// them.
    pub catalog: Arc<ToolCatalog>,
    /// The effects the process's env links: what each step's effect path
    /// runs.
    pub effects: Arc<std::collections::BTreeMap<lash_kernel_doc::EffectName, crate::EffectLink>>,
    /// The round tools over it, owned by the process.
    pub tools: Arc<dyn RoundTools>,
    /// The same composition and namespaces the round tools run under.
    pub context: Arc<dyn ProcessStepContext>,
}

/// The prepared composition a process activation judges bindings against.
/// It shares the resident component state its step bodies use, while provider
/// bindings are still judged at every admission.
#[async_trait::async_trait]
pub trait ProcessStepContext: Send + Sync {
    /// Judge the linked tool against its retained surface and current provider.
    ///
    /// # Errors
    /// The prepared runtime cannot answer the binding.
    fn step_link(
        &self,
        effect: &lash_kernel_doc::EffectName,
    ) -> Result<crate::plugin::StepLink, PluginError>;
    /// Judge a recorded dynamic match and its execution binding.
    ///
    /// # Errors
    /// The prepared runtime cannot answer the binding.
    fn matched_link(
        &self,
        matched: &crate::DeferredMatch,
    ) -> Result<(crate::plugin::StepLink, Result<(), String>), PluginError>;
    /// The component state/config formats the resident runtime holds.
    ///
    /// # Errors
    /// The prepared runtime has no engine composition.
    fn requirements(
        &self,
    ) -> Result<std::collections::BTreeSet<crate::ObligationToken>, PluginError>;
    /// Resolve a new dynamic call through the prepared deferred provider.
    ///
    /// # Errors
    /// The path or provider cannot be resolved.
    async fn resolve_dynamic(&self, path: &str) -> Result<crate::DeferredMatch, PluginError>;
}

/// Why an activation's step tools are not there.
#[derive(Debug, thiserror::Error)]
pub enum StepToolsError {
    /// The host did not build them.
    #[error(transparent)]
    Build(#[from] PluginError),
    /// Their namespaces refuse a resolution the process's committed step
    /// outcomes carry.
    #[error(transparent)]
    State(#[from] RuntimeEffectControllerError),
}

/// What one activation of a process's actor runs its steps under: the
/// actor's claimed context, and the process's step tools, whose plugin
/// session holds the process's namespaces for as long as the activation
/// does.
///
/// The activation's [`ProcessSteps::prepare`] builds the tools before any
/// step is admitted or resumed; every step of the activation runs on the
/// same ones, so a step reduces against what the steps before it committed, and two concurrent steps that change one
/// namespace compose as a round's members do. The lifecycle publishes each
/// committed outcome's resolutions here ([`publish_state`](Self::publish_state));
/// those committed before the tools are built publish into them before any
/// step runs on them.
pub struct StepRuntime {
    cx: ActorContext,
    tools: tokio::sync::OnceCell<Arc<ProcessStepTools>>,
    resident: Mutex<Resident>,
}

/// Where a committed resolution publishes.
enum Resident {
    /// No step built the tools yet: the resolutions they publish once one
    /// does, in commit order.
    Unbuilt(Vec<StateResolution>),
    /// The tools' plugin session.
    Built(Arc<dyn RoundTools>),
}

impl StepRuntime {
    /// The runtime of one activation that claimed its actor as `cx`.
    #[must_use]
    pub fn new(cx: ActorContext) -> Self {
        Self {
            cx,
            tools: tokio::sync::OnceCell::new(),
            resident: Mutex::new(Resident::Unbuilt(Vec::new())),
        }
    }

    /// The process actor's claimed context, which step bodies run under.
    #[must_use]
    pub fn cx(&self) -> &ActorContext {
        &self.cx
    }

    /// The activation's step tools, once [`tools`](Self::tools) built them.
    #[must_use]
    pub fn built_tools(&self) -> Option<Arc<ProcessStepTools>> {
        self.tools.get().cloned()
    }

    /// The activation's step tools: built by `build` the first time, with
    /// every resolution committed so far published into them before any
    /// step runs on them.
    ///
    /// # Errors
    ///
    /// [`StepToolsError`]: `build` failed, or the tools' namespaces refuse a
    /// committed resolution. The next step builds them again.
    pub async fn tools<F, Fut>(&self, build: F) -> Result<Arc<ProcessStepTools>, StepToolsError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ProcessStepTools, PluginError>>,
    {
        self.tools
            .get_or_try_init(|| async {
                let tools = Arc::new(build().await?);
                let mut resident = self.resident.lock_recover();
                if let Resident::Unbuilt(committed) = &*resident {
                    tools.tools.publish_state(committed)?;
                }
                *resident = Resident::Built(Arc::clone(&tools.tools));
                Ok(tools)
            })
            .await
            .cloned()
    }

    /// Publish `state`, the resolutions a step's committed outcome carries,
    /// into the process's namespaces; held until the tools are built when
    /// no step built them yet.
    ///
    /// # Errors
    ///
    /// A resolution a namespace's frontier refuses.
    pub fn publish_state(
        &self,
        state: &[StateResolution],
    ) -> Result<(), RuntimeEffectControllerError> {
        match &mut *self.resident.lock_recover() {
            Resident::Unbuilt(committed) => {
                committed.extend_from_slice(state);
                Ok(())
            }
            Resident::Built(tools) => tools.publish_state(state),
        }
    }
}

/// The host's half of a process step: the catalog tool's declaration and
/// body, or the engine body its registration declares
/// ([`ProcessEngineRegistry::engine_steps`](super::ProcessEngineRegistry::engine_steps)).
/// A durable backend serving processes is given exactly one.
#[async_trait::async_trait]
pub trait ProcessSteps: Send + Sync {
    /// Admit `step` of `process` on the prepared `runtime` at `now_ms`: the policy its tool declares
    /// and the limit it runs under.
    ///
    /// # Errors
    ///
    /// [`StepRefusal`]; nothing is recorded.
    async fn admit(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal>;

    /// Build `process`'s composition and the activation's step tools into
    /// `runtime`, before the activation admits or resumes any step of it
    /// (§5.1), so no step body builds them.
    /// [`StepRefusal::CompositionUnavailable`] and [`StepRefusal::EnvRefused`]
    /// park the process and settle nothing; any other refusal is a fault the
    /// next pass meets again.
    ///
    /// # Errors
    ///
    /// [`StepRefusal`].
    async fn prepare(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
    ) -> Result<(), StepRefusal> {
        let _ = (runtime, process);
        Ok(())
    }

    /// Pure conversion of a component's whole recorded state/config view.
    /// The activation records this postimage before building any step tools.
    #[doc(hidden)]
    async fn convert_components(
        &self,
        process: &ProcessRecord,
    ) -> Result<
        Option<crate::runtime::actor::process::component_formats::ComponentFormatCut>,
        PluginError,
    > {
        let _ = process;
        Ok(None)
    }

    /// The component claim requirements `process` carries while it holds
    /// `steps` (§6.2): its environment's components' state/config format tokens. `None`
    /// when it keeps none. The activation adds the callbacks its unsettled
    /// steps were admitted under (S7).
    ///
    /// # Errors
    ///
    /// The composition cannot answer; the pass is retried.
    async fn requirements(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
        steps: &[StepRequest],
    ) -> Result<
        Option<std::collections::BTreeSet<crate::ObligationToken>>,
        lash_durable::DurableError,
    > {
        let _ = (runtime, process, steps);
        Ok(None)
    }

    /// How `process`'s engine carries it onto a successor pair its host
    /// names (D2): what its registration declares, or `None`, which refuses
    /// every successor request.
    fn succession(&self, process: &ProcessRecord) -> Option<Arc<dyn super::EngineSuccession>> {
        let _ = process;
        None
    }

    /// How long a step body whose token was cancelled may still answer:
    /// the runtime's
    /// [`ExecutionBudgets::stop_grace`](lash_protocol_types::ExecutionBudgets::stop_grace).
    fn stop_grace(&self) -> std::time::Duration;

    /// The most catalog tool steps `process` may hold at once: the
    /// `max_tool_calls` its start's environment records (FIG-4546). A round
    /// of steps counts whole while any of its tool steps runs. `None`
    /// declares no limit, which a steps implementation with no recorded
    /// session policy answers.
    ///
    /// # Errors
    ///
    /// [`StepRefusal::Unavailable`] when the environment cannot be read now.
    async fn max_tool_calls(
        &self,
        process: &ProcessRecord,
    ) -> Result<Option<lash_protocol_types::MaxToolCalls>, StepRefusal> {
        let _ = process;
        Ok(None)
    }

    /// The body of `execution`, an attempt of the admitted `step` of
    /// `process`, which runs under `runtime`, the process actor's
    /// activation. It runs only after its admission (or its retry's start)
    /// committed, under its limit and the cancel token it is handed; a
    /// store-local effect it answers commits with its outcome. A step
    /// admitted with a wait parks by answering `Waiting` on the wait its
    /// execution pinned ([`ExecutionDraft::pinned_wait`]). An engine body the
    /// host lacked capacity for answers its
    /// [`CapacityVerdict`](super::CapacityVerdict) in
    /// [`MemberOutcome::capacity`](crate::runtime::actor::round::MemberOutcome::capacity),
    /// under the soundness rule the verdict states: honoured for a
    /// `Repeatable` step, it records nothing and runs again uncounted on a
    /// later claim.
    ///
    /// [`ExecutionDraft::pinned_wait`]: crate::runtime::actor::round::ExecutionDraft::pinned_wait
    fn body(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
    ) -> MemberBody;

    /// The outcome `step`'s admission settled (D3) commits as its call
    /// `call`, retained as its body's outcome would be. Steps normally
    /// commit it as their admission answered it.
    fn admitted_output(
        &self,
        process: &ProcessId,
        step: &StepRequest,
        call: &crate::ToolCallId,
        output: SettledOutput,
    ) -> SettledOutput {
        let _ = (process, step, call);
        output
    }

    /// The engine's view of a committed step outcome at incorporation.
    /// This pure projection runs no body. Steps normally answer their stored
    /// output directly; production catalog steps retain a ToolCallRecord and
    /// expose its output here.
    fn engine_output(
        &self,
        _process: &ProcessId,
        _step: &StepRequest,
        output: SettledOutput,
    ) -> SettledOutput {
        output
    }

    /// The final answer of `execution`, an attempt of `step` of `process`
    /// that parked as `parked`, once one of its waits ended with
    /// `resolution`: a pure function of the resolution and the payload of
    /// the parked outcome's material. Runs no body.
    fn resolved(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput;
}

/// A parked process call retains the request beside its completion metadata,
/// so its eventual ToolCallRecord needs no current catalog or live context.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ParkedProcessToolCall {
    call: crate::sansio::PendingToolCall,
    parked: SettledOutput,
}

/// A catalog tool's round answer retained as the one ToolCallRecord for its
/// process. The engine receives only its output, at incorporation.
#[must_use]
pub fn tool_step_output(
    process: &ProcessId,
    call: &crate::sansio::PendingToolCall,
    mut result: MemberOutcome,
) -> MemberOutcome {
    result.output = match result.output {
        SettledOutput::Waiting(parked) => {
            let source = parked.named().clone();
            let retained = ParkedProcessToolCall {
                call: call.clone(),
                parked: SettledOutput::Waiting(parked),
            };
            match serde_json::to_string(&retained) {
                Ok(text) => {
                    let mut material = process_material(process, text).parked(source.wait);
                    if let Some(terminal) = source.terminal {
                        material.await_terminal(terminal);
                    }
                    SettledOutput::Waiting(material)
                }
                Err(_) => SettledOutput::Interrupted,
            }
        }
        settled => {
            let record = settled
                .payload()
                .and_then(decode_completed)
                .map(|completed| crate::ToolCallRecord {
                    call_id: completed.call_id,
                    provider_call_id: completed.provider_call_id,
                    tool: completed.tool_name,
                    args: completed.args,
                    output: completed.output,
                });
            replace_payload(
                process,
                settled,
                record.and_then(|record| serde_json::to_string(&crate::Durable(&record)).ok()),
            )
        }
    };
    result
}

fn process_material(process: &ProcessId, text: String) -> Material {
    Material::journal_local(
        MaterialOwner::Process {
            process_id: process.clone(),
        },
        MaterialRole::AttemptOutput,
        text,
    )
}

fn replace_payload(
    process: &ProcessId,
    settled: SettledOutput,
    text: Option<String>,
) -> SettledOutput {
    match (settled, text) {
        (SettledOutput::Completed(_), Some(text)) => {
            SettledOutput::Completed(process_material(process, text))
        }
        (SettledOutput::Failed(failure), Some(text)) => {
            let (failure, _) = failure.into_parts();
            SettledOutput::Failed(
                process_material(process, text).failure(failure.reason, failure.suggested_delay_ms),
            )
        }
        (SettledOutput::Completed(_) | SettledOutput::Failed(_), None) => {
            SettledOutput::Interrupted
        }
        (settled, _) => settled,
    }
}

/// The ToolCallRecord `call` retains for `settled`, the tool failure its
/// admission answered without a body (D3).
#[must_use]
pub fn tool_step_refused(
    process: &ProcessId,
    call: &crate::sansio::PendingToolCall,
    settled: SettledOutput,
) -> SettledOutput {
    let record = settled
        .payload()
        .and_then(|payload| serde_json::from_str::<crate::ToolCallOutput>(payload).ok())
        .map(|output| crate::ToolCallRecord {
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool: call.tool_name.clone(),
            args: call.args.clone(),
            output,
        });
    replace_payload(
        process,
        settled,
        record.and_then(|record| serde_json::to_string(&crate::Durable(&record)).ok()),
    )
}

/// The committed outcome of the resolution `call` of a `call_dynamic` call
/// (P6): its ToolCallRecord, whose value is the `matched` decision, which
/// the step that consumes it never resolves again.
#[must_use]
pub fn dynamic_resolution(
    process: &ProcessId,
    call: &crate::sansio::PendingToolCall,
    matched: &crate::DeferredMatch,
) -> SettledOutput {
    let record = serde_json::to_value(matched)
        .ok()
        .map(|value| crate::ToolCallRecord {
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool: call.tool_name.clone(),
            args: call.args.clone(),
            output: crate::ToolCallOutput::success(value),
        });
    match record.and_then(|record| serde_json::to_string(&crate::Durable(&record)).ok()) {
        Some(text) => SettledOutput::Completed(process_material(process, text)),
        None => SettledOutput::Interrupted,
    }
}

/// The match a resolution step's committed `output` recorded, or `None`
/// for a resolution that settled without one (its input was refused, or
/// its body did not answer).
#[must_use]
pub fn resolved_match(output: &SettledOutput) -> Option<crate::DeferredMatch> {
    let SettledOutput::Completed(material) = output else {
        return None;
    };
    let record = serde_json::from_str::<crate::ToolCallRecord>(material.payload()).ok()?;
    match record.output.outcome {
        crate::ToolCallOutcome::Success(_) => {
            serde_json::from_value(record.output.value_for_projection()).ok()
        }
        _ => None,
    }
}

/// The call of the tool `matched` resolved `resolution`'s path to: the
/// step lash issues in the commit that consumes the resolution, under the
/// same step name and site, with the call's `args` as its input. `None`
/// when `matched` resolved nothing, or `resolution` is not a resolution.
#[must_use]
pub fn dynamic_target(
    resolution: &StepRequest,
    matched: crate::DeferredMatch,
) -> Option<StepRequest> {
    let StepRequest::Tool {
        step,
        effect,
        input,
        site,
        matched: None,
    } = resolution
    else {
        return None;
    };
    matched.resolved()?;
    let call = serde_json::from_value::<crate::DynamicCall>(input.clone()).ok()?;
    Some(StepRequest::Tool {
        step: step.clone(),
        effect: effect.clone(),
        input: call.args,
        site: site.clone(),
        matched: Some(Box::new(matched)),
    })
}

/// What a body that performed `call_dynamic` is answered when its
/// resolution decided `matched` against it: a refused or unavailable path
/// is a tool failure it may catch, typed, from the recorded decision alone.
/// `None` for a resolved match, whose target answers instead.
#[must_use]
pub fn dynamic_decision(
    process: &ProcessId,
    matched: &crate::DeferredMatch,
) -> Option<SettledOutput> {
    let path = matched.requested.clone();
    let failure = match &matched.outcome {
        crate::DeferredOutcome::Resolved { .. } => return None,
        crate::DeferredOutcome::Refused { reason } => crate::ToolFailure::runtime(
            crate::ToolFailureClass::PermissionDenied,
            "dynamic_call_refused",
            format!("the call path `{path}` was refused: {reason}"),
        )
        .with_cause(crate::ToolFailureCause::DynamicCallRefused {
            path,
            reason: reason.clone(),
        }),
        crate::DeferredOutcome::Unavailable { note } => crate::ToolFailure::runtime(
            crate::ToolFailureClass::Unavailable,
            "dynamic_call_unavailable",
            match note {
                Some(note) => format!("no tool is available for the call path `{path}`: {note}"),
                None => format!("no tool is available for the call path `{path}`"),
            },
        )
        .with_cause(crate::ToolFailureCause::DynamicCallUnavailable {
            path,
            note: note.clone(),
        }),
    };
    let text =
        serde_json::to_string(&crate::Durable(&crate::ToolCallOutput::failure(failure))).ok()?;
    Some(SettledOutput::Failed(
        process_material(process, text).failure(
            lash_core_store::tool_run::KnownFailureReason::Reported,
            None,
        ),
    ))
}

/// Project a retained process tool record to the output its engine reads.
#[must_use]
pub fn tool_step_settled(process: &ProcessId, settled: SettledOutput) -> SettledOutput {
    let output = settled
        .payload()
        .and_then(|payload| serde_json::from_str::<crate::ToolCallRecord>(payload).ok())
        .and_then(|record| serde_json::to_string(&crate::Durable(&record.output)).ok());
    replace_payload(process, settled, output)
}

/// A parked process call's retained ToolCallRecord, rebuilt from its pinned
/// request and the resolution of its completion wait.
#[must_use]
pub fn tool_step_resolved(
    process: &ProcessId,
    parked: &Material<CompletionSource>,
    resolution: Resolution,
) -> SettledOutput {
    let Ok(retained) = serde_json::from_str::<ParkedProcessToolCall>(parked.payload()) else {
        return SettledOutput::Interrupted;
    };
    let SettledOutput::Waiting(parked) = retained.parked else {
        return SettledOutput::Interrupted;
    };
    let output = crate::tool_dispatch::parked_call_output(&parked, resolution);
    let record = crate::ToolCallRecord {
        call_id: retained.call.call_id,
        provider_call_id: retained.call.provider_call_id,
        tool: retained.call.tool_name,
        args: retained.call.args,
        output,
    };
    match serde_json::to_string(&crate::Durable(&record)) {
        Ok(text) => SettledOutput::Completed(process_material(process, text)),
        Err(_) => SettledOutput::Interrupted,
    }
}

/// Read one retained process tool record without driving its owner or
/// consulting its current catalog. Unknown, unfinished or unmaterialized
/// calls have no retained record.
///
/// # Errors
/// Failed durable reads or corrupt committed records.
pub async fn read_process_tool_call(
    host: &ActorContext,
    process_id: &ProcessId,
    call_id: &crate::ToolCallId,
) -> Result<Option<crate::ToolCallRecord>, lash_durable::DurableError> {
    use crate::runtime::actor::round::{self, PolicyView};
    use lash_durable::domain::OwnerKey;
    use lash_durable::{DurableError, StoreFailure, StoreFailureKind};

    let corrupt = |message| {
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message,
        })
    };
    let reads = host.durable_reads()?;
    let rows = reads
        .run_facts(&OwnerKey::Process(process_id.clone()))
        .await?;
    let waits = round::PinnedWaits::read(reads, &rows).await?;
    let fold = round::fold(&rows, &PolicyView::new([]), &waits)
        .map_err(|error| corrupt(format!("the process's tool records: {error}")))?;
    for member in fold.rounds().flat_map(|round| round.members()) {
        if member.call() != call_id {
            continue;
        }
        let Some(payload) = member
            .outcome()
            .and_then(lash_core_store::tool_run::SettledOutput::payload)
        else {
            return Ok(None);
        };
        let record: crate::ToolCallRecord = serde_json::from_str(payload).map_err(|error| {
            corrupt(format!(
                "call {call_id} has no readable tool record: {error}"
            ))
        })?;
        if record.call_id != *call_id {
            return Err(corrupt(format!(
                "call {call_id} has another call's recorded answer"
            )));
        }
        return Ok(Some(record));
    }
    Ok(None)
}

/// One step of a process as its run journal retains it (FIG-6370).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedProcessStep {
    pub call: crate::ToolCallId,
    /// The admission the step belongs to: the run its call id names.
    pub run: u64,
    /// The admitted tool: an engine step's is `{engine}/{kind}`.
    pub tool: String,
    /// Its final outcome, once it has one.
    pub outcome: Option<SettledOutput>,
    /// The run fact that records that outcome: what owns the heap pages
    /// the outcome published (FIG-6193), and what a reader leases them
    /// under while the journal retains it.
    pub outcome_fact: Option<StepFact>,
}

/// Where one run fact of a process's journal stands, and its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepFact {
    pub run: lash_durable::domain::RunSeq,
    pub ordinal: lash_durable::domain::Ordinal,
    pub record: lash_core_store::checkpoint_graph::NodeId,
}

/// The call id of the process step `step` admitted in `run`: a reader that
/// knows a step's name finds its record by it.
#[must_use]
pub fn process_step_call(
    process_id: &ProcessId,
    step: &super::StepName,
    run: u64,
) -> crate::ToolCallId {
    lash_protocol_types::ToolCallAdmission::process("", process_id.clone())
        .call_id(&[lash_protocol_types::ToolCallPosition::ProcessStep { step: &step.0, run }])
}

/// Read every step a process's run journal retains, in admission order,
/// without driving its owner. A pruned process retains none.
///
/// # Errors
/// Failed durable reads or corrupt committed records.
pub async fn read_process_steps(
    host: &ActorContext,
    process_id: &ProcessId,
) -> Result<Vec<RecordedProcessStep>, lash_durable::DurableError> {
    use crate::runtime::actor::round::{self, PolicyView};
    use lash_durable::domain::OwnerKey;
    use lash_durable::{DurableError, StoreFailure, StoreFailureKind};

    let reads = host.durable_reads()?;
    let rows = reads
        .run_facts(&OwnerKey::Process(process_id.clone()))
        .await?;
    let waits = round::PinnedWaits::read(reads, &rows).await?;
    let fold = round::fold(&rows, &PolicyView::new([]), &waits).map_err(|error| {
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message: format!("the process's step records: {error}"),
        })
    })?;
    let facts = &rows.rows;
    Ok(fold
        .rounds()
        .flat_map(|round| {
            round
                .members()
                .iter()
                .map(move |member| RecordedProcessStep {
                    call: member.call().clone(),
                    run: round.run().0,
                    tool: member.draft().tool().as_str().to_owned(),
                    outcome: member.outcome().cloned(),
                    outcome_fact: facts
                        .iter()
                        .rev()
                        .find(|row| {
                            row.run == round.run()
                                && row.kind == lash_durable::domain::RunFactKind::XOutcome
                                && row.call.as_ref() == Some(member.call())
                        })
                        .map(|row| StepFact {
                            run: row.run,
                            ordinal: row.ordinal,
                            record: row.record,
                        }),
                })
        })
        .collect())
}
