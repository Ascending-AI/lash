//! A host process engine's state machine (ADR 0132 §7; S6 of I0, FIG-5194).
//!
//! An engine never performs an effect. Lash calls
//! [`ProcessEngine::advance`](super::ProcessEngine::advance) with the
//! process's committed state and one event; the engine answers its next state
//! and one action. The new state and the admission of the action (started
//! step rows, a wait row, a timer) commit in one `process.advance`
//! transaction before the action runs, so an uncommitted transition is
//! recomputed by calling `advance` again with the same state and event.
//!
//! **Cancel.** A running or waiting process receives
//! [`EngineEvent::Cancelled`] once, within its grace; the engine may answer
//! with best-effort [`EngineAction::Steps`], which run within the grace, or
//! with [`EngineAction::Terminal`]. At `grace_until` lash commits a forced
//! cancellation without calling `advance` again. A parked process, or one
//! whose state this node cannot decode, ends engine-free: its claimer commits
//! the terminal from registry state and calls nothing.
//!
//! **Signals** reach `advance` as [`EngineEvent::Signal`], in mailbox order
//! per sender; concurrent senders are unordered.
//!
//! L6 (FIG-5175) drives this machine from the process activation.

use std::time::Duration;

use lash_durable::DurableInstant;

use crate::runtime::actor::round::SettledOutput;
use crate::runtime::actor::waits::PinnedKey;
use crate::{ProcessId, ProcessOutcome, ProcessSignal, Resolution};

/// The encoding of an engine's [`EngineState`]: the engine's kind and a
/// version it bumps when the encoding changes. A node claims a process only
/// when its engine reads the process's format (L11, FIG-5187).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct EngineStateFormat {
    /// The engine kind that wrote the state.
    pub kind: String,
    /// The engine's encoding version.
    pub version: u32,
}

/// A process's committed engine state: opaque bytes in a declared format.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EngineState {
    /// The encoding of `bytes`.
    pub format: EngineStateFormat,
    /// The engine's own encoding of its state.
    pub bytes: Vec<u8>,
}

impl EngineState {
    /// The state of a process before [`EngineEvent::Started`]: no bytes.
    #[must_use]
    pub fn empty(format: EngineStateFormat) -> Self {
        Self {
            format,
            bytes: Vec::new(),
        }
    }
}

/// The name an engine gives one of its waits; unique within the process.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct KeyName(pub String);

/// The name an engine gives one of its steps; with the step's ordinal and the
/// process, it is the step's identity.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct StepName(pub String);

/// What kind of host-resolvable wait an engine pins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HostWaitKind {
    /// A tool's completion, resolved by the host with its key.
    ToolCompletion,
    /// A host-defined wait, resolved by the host with its key.
    Custom,
}

/// One step an engine asks lash to run. Its identity is
/// `(process, step, ordinal)`, and its `ToolCallId` derives from it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StepRequest {
    /// A catalog tool, under its declaration, `ExecutionPolicy` and ceiling.
    Tool {
        /// The step's name.
        step: StepName,
        /// The catalog tool it runs.
        tool: lash_sansio::ToolId,
        /// The tool's input.
        input: serde_json::Value,
        /// The effect node it runs for, when the engine's execution map
        /// names one: its committed outcome is recorded as that node's
        /// `process.effect_outcome`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<StepEffectSite>,
        /// The language node that issued this call. The admitted execution
        /// supplies its call id when the body starts, after admission commits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language_execution: Option<Box<lash_trace::TraceLanguageExecution>>,
    },
    /// A body of the process's own engine, run through the [`EngineSteps`]
    /// its registration declares, under a pinned `Repeatable` policy. It is
    /// neither model-visible nor host-invocable. A registration that
    /// declares no engine steps, or not this kind, is refused before
    /// admission ([`EngineStepRefusal`]).
    Engine {
        /// The step's name.
        step: StepName,
        /// Which of the engine's bodies runs.
        kind: EngineStepKind,
        /// The body's input.
        input: serde_json::Value,
    },
    /// An operation on the lash store that no catalog tool answers (a
    /// lashlang process's trigger command), run through the
    /// [`EngineHostSteps`] its engine's registration declares, admitted
    /// `Once`. A registration that declares none, or not this operation, is
    /// refused before admission ([`EngineStepRefusal`]).
    Host {
        /// The step's name.
        step: StepName,
        /// The host operation it performs.
        operation: String,
        /// The operation's input.
        input: serde_json::Value,
        /// The effect node it runs for, as for a tool step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<StepEffectSite>,
        /// The language node that issued this call, bound by its admitted
        /// execution when the host step's body starts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language_execution: Option<Box<lash_trace::TraceLanguageExecution>>,
    },
}

/// The effect node of an engine's execution map that a step runs for, and
/// which occurrence of that node it is: what the step's committed outcome
/// is recorded under (`process.effect_outcome`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEffectSite {
    /// The node's id in the engine's execution map.
    pub node_id: String,
    /// Which occurrence of the node this is, from 1.
    pub occurrence: u64,
}

impl StepRequest {
    /// The step's name.
    #[must_use]
    pub fn step(&self) -> &StepName {
        match self {
            Self::Tool { step, .. } | Self::Engine { step, .. } | Self::Host { step, .. } => step,
        }
    }

    /// The step's input: the tool's, or the engine body's.
    #[must_use]
    pub fn input(&self) -> &serde_json::Value {
        match self {
            Self::Tool { input, .. } | Self::Engine { input, .. } | Self::Host { input, .. } => {
                input
            }
        }
    }

    /// The effect node the step runs for, if its engine named one.
    #[must_use]
    pub fn site(&self) -> Option<&StepEffectSite> {
        match self {
            Self::Tool { site, .. } | Self::Host { site, .. } => site.as_ref(),
            Self::Engine { .. } => None,
        }
    }

    /// The tool its admission records for a process of engine `engine`:
    /// the catalog tool, or for an engine step `<engine>/<kind>` and for a
    /// host step `<engine>/<operation>`, which only identify the record. An
    /// engine or host step is dispatched by its variant, never by this name.
    #[must_use]
    pub fn admitted_tool(&self, engine: &str) -> lash_sansio::ToolId {
        match self {
            Self::Tool { tool, .. } => tool.clone(),
            Self::Engine { kind, .. } => lash_sansio::ToolId::new(format!("{engine}/{}", kind.0)),
            Self::Host { operation, .. } => {
                lash_sansio::ToolId::new(format!("{engine}/{operation}"))
            }
        }
    }
}

/// The name an engine gives one of its own step bodies; meaningful only to
/// that engine's [`EngineSteps`].
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct EngineStepKind(pub String);

impl EngineStepKind {
    /// The kind named `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

/// What an engine step's body is handed: the process, what its row
/// recorded, the catalog its steps resolve against, the activation's clock
/// reading and the step's own kind and input.
#[derive(Clone)]
pub struct EngineStepRun {
    /// The process the step belongs to.
    pub process: ProcessId,
    /// What the engine's `creation_config` recorded on the process's row.
    pub engine_config: Option<serde_json::Value>,
    /// The catalog the process's steps resolve against.
    pub tool_catalog: std::sync::Arc<crate::ToolCatalog>,
    /// The activation's clock reading when the body started.
    pub now: DurableInstant,
    /// The activation's clock, for a body that waits.
    pub clock: std::sync::Arc<dyn crate::Clock>,
    /// The backend's projection providers, which a body that runs a VM reads
    /// projections through (ADR 0132 §9).
    pub projection_providers:
        Option<std::sync::Arc<dyn crate::runtime::actor::projection::ProjectionProviders>>,
    /// Which body runs.
    pub kind: EngineStepKind,
    /// The body's input.
    pub input: serde_json::Value,
}

/// An engine's own step bodies, declared at registration
/// ([`ProcessEngineRegistration::with_engine_steps`](super::ProcessEngineRegistration::with_engine_steps)).
/// Each runs under a pinned `Repeatable` policy: a crash before its outcome
/// commits runs it again from the same input, so a body must be a
/// recomputation from that input, with no effect of its own.
#[async_trait::async_trait]
pub trait EngineSteps: Send + Sync {
    /// The kinds of body this engine runs.
    fn kinds(&self) -> Vec<EngineStepKind>;

    /// Run one body to its outcome, observing `cancel`.
    async fn run(
        &self,
        run: EngineStepRun,
        cancel: tokio_util::sync::CancellationToken,
    ) -> SettledOutput;
}

/// What a host step's body is handed: the step's lash-minted call identity,
/// which keys its store effect, and its operation and input.
#[derive(Clone, Debug, PartialEq)]
pub struct HostStepRun {
    /// The step's call identity.
    pub call: crate::ToolCallId,
    /// The host operation it performs.
    pub operation: String,
    /// The operation's input.
    pub input: serde_json::Value,
}

/// An engine's host steps, declared at registration
/// ([`ProcessEngineRegistration::with_host_steps`](super::ProcessEngineRegistration::with_host_steps)):
/// operations on the lash store its processes issue that no catalog tool
/// answers. Each is admitted `Once` and runs once over its process's step
/// execution context, which acts as the process's recorded originator. Its
/// store write is its store-local effect (ADR 0132 §5): a crash before its
/// outcome commits records `Interrupted`, never a second write.
#[async_trait::async_trait]
pub trait EngineHostSteps: Send + Sync {
    /// Whether `operation` is one of this engine's host steps.
    fn serves(&self, operation: &str) -> bool;

    /// Run one host step to its answer over `context`, the process's step
    /// execution context.
    async fn run(
        &self,
        context: crate::RuntimeExecutionContext<'static>,
        run: HostStepRun,
    ) -> crate::ToolCallOutput;
}

/// Why an engine step was refused before admission.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineStepRefusal {
    /// No engine of this kind is registered.
    #[error("no process engine `{engine}` is registered")]
    UnknownEngine {
        /// The engine kind.
        engine: String,
    },
    /// The engine's registration declares no engine steps.
    #[error("process engine `{engine}` declares no engine steps")]
    NoEngineSteps {
        /// The engine kind.
        engine: String,
    },
    /// The engine's steps do not include this kind.
    #[error("process engine `{engine}` declares no engine step `{kind:?}`")]
    UndeclaredStep {
        /// The engine kind.
        engine: String,
        /// The step kind asked for.
        kind: EngineStepKind,
    },
    /// The engine's registration declares no host step for this
    /// operation.
    #[error("process engine `{engine}` declares no host step `{operation}`")]
    UndeclaredHostStep {
        /// The engine kind.
        engine: String,
        /// The host operation asked for.
        operation: String,
    },
}

/// What an engine asks lash to do next.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineAction {
    /// Run these steps; non-empty. Each completion is one
    /// [`EngineEvent::StepSettled`].
    Steps(Vec<StepRequest>),
    /// Pin a host-resolvable key, minted before any step submits it.
    PinKey {
        /// The wait's name.
        name: KeyName,
        /// The wait's kind.
        kind: HostWaitKind,
        /// How long the wait may stay open.
        deadline: Option<Duration>,
    },
    /// Wait for a pinned key's resolution.
    AwaitExternal {
        /// The wait's name.
        name: KeyName,
    },
    /// Wait for another process's terminal.
    AwaitProcess {
        /// The awaited process.
        process: ProcessId,
        /// How long the wait may stay open.
        deadline: Option<Duration>,
    },
    /// Sleep until a durable instant.
    Sleep {
        /// When the process wakes.
        until: DurableInstant,
    },
    /// Nothing to do and no deadline: the process waits until the next
    /// mailbox event (a signal, `Cancelled`, a resolved wait or a settled
    /// step) reaches `advance`.
    Idle,
    /// Wait for the signal named `name`: as [`Idle`](Self::Idle), the
    /// process waits for its next mailbox event, and its record shows it
    /// waiting on the signal (`process.waiting`) until a transition asks for
    /// anything else (`process.resumed`).
    AwaitSignal {
        /// The signal's name.
        name: String,
    },
    /// Append a process event, exactly once, in the transaction that
    /// commits this state; `advance` then receives [`EngineEvent::Emitted`]
    /// at once.
    Emit {
        /// The event's type.
        event_type: crate::ProcessEventType,
        /// Its payload.
        payload: serde_json::Value,
    },
    /// End the process.
    Terminal(ProcessOutcome),
}

/// What happened to a process since its last transition.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    /// The process started with its payload.
    Started {
        /// The start payload.
        payload: serde_json::Value,
    },
    /// One requested step settled.
    StepSettled {
        /// The step's name.
        step: StepName,
        /// How its attempt ended, with its material's payload.
        outcome: SettledOutput,
    },
    /// The event the last transition's [`EngineAction::Emit`] appended is
    /// committed.
    Emitted,
    /// A key the engine asked for was pinned.
    KeyPinned {
        /// The wait's name.
        name: KeyName,
        /// The pinned key.
        key: PinnedKey,
    },
    /// A pinned key was resolved.
    ExternalResolved {
        /// The wait's name.
        name: KeyName,
        /// The resolution.
        resolution: Resolution,
    },
    /// A pinned key's deadline passed.
    ExternalTimedOut {
        /// The wait's name.
        name: KeyName,
    },
    /// An awaited process ended.
    ProcessEnded {
        /// The awaited process.
        process: ProcessId,
        /// Its outcome.
        outcome: ProcessOutcome,
    },
    /// An awaited process's deadline passed.
    ProcessWaitTimedOut {
        /// The awaited process.
        process: ProcessId,
    },
    /// A sleep ended.
    Woke,
    /// A signal arrived.
    Signal(ProcessSignal),
    /// The process was cancelled; delivered once, within its grace.
    Cancelled {
        /// Who asked.
        origin: lash_sansio::CancelOrigin,
        /// When lash forces the terminal.
        grace_until: DurableInstant,
    },
}
