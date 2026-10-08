//! How a host process engine's steps run (ADR 0132 §10; L6, FIG-5175).
//!
//! A [`StepRequest`] names a catalog tool, one of the engine's own bodies
//! that its registration declares ([`EngineSteps`](super::EngineSteps)), or
//! one of its host steps ([`EngineHostSteps`](super::EngineHostSteps)). The
//! process actor admits it as an admitted execution (S4) under the tool's
//! declared [`ExecutionPolicy`] (an engine body's is a pinned `Repeatable`, a
//! host step's `Once`) and an [`ExecutionLimit`] within the tool ceiling,
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
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy};

use lash_core_store::tool_run::{MaterialOwner, MaterialRole};

use super::engine_state::StepRequest;
use crate::runtime::actor::round::{
    AdmittedExecution, Material, MemberBody, MemberResult, RoundTools, SettledOutput,
    decode_completed,
};
use crate::runtime::actor::waits::{Resolution, WaitDeadline};
use crate::{
    ActorContext, PluginError, ProcessId, ProcessRecord, RuntimeEffectControllerError, ToolCatalog,
};

/// The policy every engine step is admitted under: `Repeatable`, so a crash
/// before its outcome commits runs the body again from the same input, at
/// most this many attempts, with no backoff between them.
#[must_use]
pub fn engine_step_policy() -> ExecutionPolicy {
    ExecutionPolicy::repeatable(ENGINE_STEP_ATTEMPTS, 0, 0)
}

/// The attempts an engine step's body may take.
const ENGINE_STEP_ATTEMPTS: std::num::NonZeroU32 = std::num::NonZeroU32::MIN.saturating_add(2);

/// A step's admission: what its tool's declaration pins before it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepAdmission {
    /// The tool's execution policy.
    pub policy: ExecutionPolicy,
    /// The step's limit, within the tool ceiling.
    pub limit: ExecutionLimit,
    /// For a step that may park, the deadline of the completion wait its
    /// admission pins: its park never outlives it.
    pub wait: Option<WaitDeadline>,
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
    Engine(#[from] super::engine_state::EngineStepRefusal),
}

/// The tools a process's tool steps run: the catalog they resolve against,
/// and the round tools that pin a catalog tool's policy and limit and run
/// its body through the tool dispatch a turn's round runs on, on the
/// process's own plugin session.
pub struct ProcessStepTools {
    /// The process's own plugin session's catalog.
    pub catalog: Arc<ToolCatalog>,
    /// The round tools over it, owned by the process.
    pub tools: Arc<dyn RoundTools>,
    /// The execution context a host step runs over: the round tools'
    /// dispatch, acting as the process's recorded originator.
    pub host: crate::RuntimeExecutionContext<'static>,
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
/// The first tool step that runs builds the tools; every later step of
/// the activation runs on the same ones, so a step reduces against what the
/// steps before it committed, and two concurrent steps that change one
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
    /// Admit `step` of `process` at `now_ms`: the policy its tool declares
    /// and the limit it runs under.
    ///
    /// # Errors
    ///
    /// [`StepRefusal`]; nothing is recorded.
    async fn admit(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal>;

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
    ) -> Result<Option<lash_sansio::MaxToolCalls>, StepRefusal> {
        let _ = process;
        Ok(None)
    }

    /// The body of `execution`, an attempt of the admitted `step` of
    /// `process`, which runs under `runtime`, the process actor's
    /// activation. It runs only after its admission (or its retry's start)
    /// committed, under its limit and the cancel token it is handed; a
    /// store-local effect it answers commits with its outcome. A step
    /// admitted with a wait parks by answering `Waiting` on the wait its
    /// execution pinned ([`ExecutionDraft::pinned_wait`]).
    ///
    /// [`ExecutionDraft::pinned_wait`]: crate::runtime::actor::round::ExecutionDraft::pinned_wait
    fn body(
        &self,
        runtime: &Arc<StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
    ) -> MemberBody;

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

/// A catalog tool's round answer as `process`'s step answer: the call's
/// `ToolCallOutput` as the outcome's material, owned by the process, where
/// the round's names the whole answered call. A step's payload is its tool
/// output, which is what its engine reads.
#[must_use]
pub fn tool_step_output(process: &ProcessId, mut result: MemberResult) -> MemberResult {
    result.output = tool_step_settled(process, result.output);
    result
}

/// [`tool_step_output`]'s answer, for a settled output alone.
#[must_use]
pub fn tool_step_settled(process: &ProcessId, settled: SettledOutput) -> SettledOutput {
    let output = settled
        .payload()
        .and_then(decode_completed)
        .and_then(|completed| serde_json::to_string(&completed.output).ok());
    let material = |text: String| {
        Material::journal_local(
            MaterialOwner::Process {
                process_id: process.clone(),
            },
            MaterialRole::AttemptOutput,
            text,
        )
    };
    match (settled, output) {
        (SettledOutput::Completed(_), Some(text)) => SettledOutput::Completed(material(text)),
        (SettledOutput::Failed(failure), Some(text)) => {
            let (failure, _) = failure.into_parts();
            SettledOutput::Failed(
                material(text).failure(failure.reason, failure.suggested_delay_ms),
            )
        }
        // An answer that does not re-encode reached no durable form: the
        // call may or may not have taken effect.
        (SettledOutput::Completed(_) | SettledOutput::Failed(_), None) => {
            SettledOutput::Interrupted
        }
        (settled, _) => settled,
    }
}

/// A host step's answer as `process`'s step answer: `output` as the
/// outcome's material, owned by the process. A host step's answer is final,
/// failure or not: it never repeats.
#[must_use]
pub fn host_step_output(process: &ProcessId, output: &crate::ToolCallOutput) -> SettledOutput {
    match serde_json::to_string(output) {
        Ok(text) => SettledOutput::Completed(Material::journal_local(
            MaterialOwner::Process {
                process_id: process.clone(),
            },
            MaterialRole::AttemptOutput,
            text,
        )),
        // An answer that does not encode reached no durable form: the
        // operation may or may not have taken effect.
        Err(_) => SettledOutput::Interrupted,
    }
}

/// The completion wait of a catalog tool step that may defer: one limit
/// spans its body and its park, as a round member's does.
#[must_use]
pub fn tool_step_wait(limit: &ExecutionLimit) -> WaitDeadline {
    WaitDeadline::at_instant(lash_durable::DurableInstant(
        i64::try_from(limit.expires_at).unwrap_or(i64::MAX),
    ))
}

/// The answer of `process`'s catalog tool step that parked as `parked`,
/// once one of its waits ended with `resolution`: the tool output the
/// resolution answers, as the step's material.
#[must_use]
pub fn tool_step_resolved(
    process: &ProcessId,
    parked: &Material<CompletionSource>,
    resolution: Resolution,
) -> SettledOutput {
    let output = crate::tool_dispatch::parked_call_output(parked, resolution);
    match serde_json::to_string(&output) {
        Ok(text) => SettledOutput::Completed(Material::journal_local(
            MaterialOwner::Process {
                process_id: process.clone(),
            },
            MaterialRole::AttemptOutput,
            text,
        )),
        // An answer that does not encode reached no durable form.
        Err(_) => SettledOutput::Interrupted,
    }
}
