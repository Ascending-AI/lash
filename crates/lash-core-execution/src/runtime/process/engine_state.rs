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
//! L6 (FIG-5175) drives this machine from the process activation.

use std::time::Duration;

use lash_durable::DurableInstant;

use crate::runtime::actor::round::SettledOutput;
use crate::runtime::actor::waits::PinnedKey;
use crate::{ProcessId, ProcessOutcome, Resolution};

/// The encoding of an engine's [`EngineState`]: the engine's kind and a
/// version it bumps when the encoding changes. A node claims a process only
/// when its engine reads the process's format (L11, FIG-5187).
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
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

/// An engine's carrying of its state from an earlier build's format to its
/// own (ADR 0106 §1). An engine registered with one
/// ([`Backend::with_state_migration`](crate::Backend::with_state_migration))
/// decodes the formats it [`carries`](Self::carries) beside its
/// [`state_format`](super::ProcessEngine::state_format), so its node claims
/// a process the previous build left in one. The claimer carries the state
/// forward as its first commit, before any transition, once every live node
/// that serves the process decodes the newer format; until then it advances
/// the process in the format it is in.
#[async_trait::async_trait]
pub trait EngineStateMigration: Send + Sync {
    /// The earlier formats the engine carries forward.
    fn carries(&self) -> Vec<EngineStateFormat>;

    /// Whether the engine also carries a state already in its own format
    /// onto what this build holds: the adoption an operator chose of what
    /// an earlier build wrote against functions this build is about to
    /// stop holding (FIG-5799). The claimer asks
    /// [`migrate`](Self::migrate) of such a state too, once every live node
    /// that serves the process decodes this build's set; a refusal leaves
    /// the process as written.
    fn adopts(&self) -> bool {
        false
    }

    /// `state`, written in a carried format, in the engine's own, or
    /// adopted when the engine [`adopts`](Self::adopts); or
    /// `None` when the process is not at a point the engine carries it
    /// from, and goes on in the format it is in. It may read and publish
    /// what the state names, and it changes nothing of the process: the
    /// claimer commits the answer, or parks the process with the refusal.
    async fn migrate(
        &self,
        process: &ProcessId,
        state: &EngineState,
    ) -> Result<Option<EngineState>, EngineStateRefusal>;
}

/// Why an engine did not carry a state forward.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineStateRefusal {
    /// The engine's typed reason, as its own data.
    pub refusal: serde_json::Value,
    /// The reason in words.
    pub message: String,
    /// The same state will be refused again: the process is parked with
    /// the refusal. A refusal that is not final, such as a store that did
    /// not answer, is asked again at the next claim.
    pub fatal: bool,
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
        /// The site of the engine's workflow document it runs for, when the
        /// engine names one: its committed outcome is recorded as that
        /// site's `process.effect_outcome`, and the start of its admitted
        /// body is observed there ([`lash_trace::StepBodyStarted`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_sansio::EffectIdentity>,
    },
    /// A body of the process's own engine, run through the [`EngineSteps`]
    /// its registration declares, under the retry policy its kind declares
    /// (or its registration overrides), pinned at admission. It is neither
    /// model-visible nor host-invocable. A registration that
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
}

impl StepRequest {
    /// The step's name.
    #[must_use]
    pub fn step(&self) -> &StepName {
        match self {
            Self::Tool { step, .. } | Self::Engine { step, .. } => step,
        }
    }

    /// The step's input: the tool's, or the engine body's.
    #[must_use]
    pub fn input(&self) -> &serde_json::Value {
        match self {
            Self::Tool { input, .. } | Self::Engine { input, .. } => input,
        }
    }

    /// The effect node the step runs for, if its engine named one.
    #[must_use]
    pub fn site(&self) -> Option<&lash_sansio::EffectIdentity> {
        match self {
            Self::Tool { site, .. } => site.as_ref(),
            Self::Engine { .. } => None,
        }
    }

    /// The tool its admission records for a process of engine `engine`:
    /// the catalog tool, or for an engine step `<engine>/<kind>`, which only
    /// identifies the record. An engine step is dispatched by its variant,
    /// never by this name.
    #[must_use]
    pub fn admitted_tool(&self, engine: &str) -> lash_sansio::ToolId {
        match self {
            Self::Tool { tool, .. } => tool.clone(),
            Self::Engine { kind, .. } => lash_sansio::ToolId::new(format!("{engine}/{}", kind.0)),
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
/// Each runs under the retry policy its kind declares, pinned at admission:
/// under `Repeatable`, a crash before its outcome commits runs it again from
/// the same input and a failure is retried within the policy's attempts, so
/// such a body must be a recomputation from that input, with no effect of
/// its own; under `Once`, a crash settles it `Interrupted` and a failure is
/// its outcome.
#[async_trait::async_trait]
pub trait EngineSteps: Send + Sync {
    /// The kinds of body this engine runs.
    fn kinds(&self) -> Vec<EngineStepKind>;

    /// How long one run of a `kind` body may take: the engine's own bound,
    /// as a host sets a tool's. Lash holds no step default.
    fn execution(&self, kind: &EngineStepKind) -> Duration;

    /// How a `kind` body is retried after a failure or a crash: the engine
    /// author's default, which the host may override per kind when it
    /// registers the engine
    /// ([`ProcessEngineRegistration::with_engine_step_retry`](super::ProcessEngineRegistration::with_engine_step_retry)).
    /// Lash holds no retry default.
    fn retry(&self, kind: &EngineStepKind) -> lash_sansio::ExecutionPolicy;

    /// Run one body to its outcome, observing `cancel`.
    async fn run(
        &self,
        run: EngineStepRun,
        cancel: tokio_util::sync::CancellationToken,
    ) -> SettledOutput;
}

/// Why an engine step was refused before admission.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
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
}

/// What an engine asks lash to do next.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineAction {
    /// Run these steps; non-empty. Each completion is one
    /// [`EngineEvent::StepSettled`]. With a `wake`, the process also sleeps
    /// until that instant while they run, as [`Sleep`](Self::Sleep) does:
    /// [`EngineEvent::Woke`] arrives once it passes, unless a transition
    /// before then answered another action.
    Steps {
        /// The steps to run.
        steps: Vec<StepRequest>,
        /// When the process wakes while they run, if it sleeps at all.
        wake: Option<DurableInstant>,
    },
    /// Pin a host-resolvable key, minted before any step submits it.
    PinKey {
        /// The wait's name.
        name: KeyName,
        /// How long the wait may stay open: the engine's own bound, with no
        /// lash default or ceiling.
        bound: crate::ParkBound,
    },
    /// Wait for a pinned key's resolution.
    AwaitExternal {
        /// The wait's name.
        name: KeyName,
        /// The node that waits, when the engine's workflow document names one.
        site: Option<lash_sansio::EffectIdentity>,
    },
    /// Wait for another process's terminal.
    AwaitProcess {
        /// The awaited process.
        process: ProcessId,
        /// How long the wait may stay open: the engine's own bound, with no
        /// lash default or ceiling.
        bound: crate::ParkBound,
        /// The node that waits, when the engine's workflow document names one.
        site: Option<lash_sansio::EffectIdentity>,
    },
    /// Sleep until a durable instant.
    Sleep {
        /// When the process wakes.
        until: DurableInstant,
        /// The node that sleeps, when the engine's workflow document names one.
        site: Option<lash_sansio::EffectIdentity>,
    },
    /// Nothing to do and no deadline: the process waits until the next
    /// mailbox event (`Cancelled`, a resolved wait or a settled step)
    /// reaches `advance`.
    Idle,
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
        /// The admitted tool identity; engine steps have no tool call.
        call_id: Option<crate::ToolCallId>,
        /// The step's name.
        step: StepName,
        /// How its attempt ended, with its material's payload.
        outcome: SettledOutput,
    },
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
    /// The process was cancelled; delivered once, within its grace.
    Cancelled {
        /// Who asked.
        origin: lash_sansio::CancelOrigin,
        /// When lash forces the terminal.
        grace_until: DurableInstant,
    },
}
