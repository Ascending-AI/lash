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

use lash_core_store::resolutions::{ExecutionId, ResolutionEntry};
use lash_durable::DurableInstant;

use crate::ProcessOutcome;
use crate::Resolution;
use crate::waits::PinnedKey;
use lash_core_store::tool_run::SettledOutput;
use lash_vocabulary::identity::ProcessId;

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

impl EngineState {
    /// The state of a process before [`EngineEvent::Started`]: no bytes.
    #[must_use]
    pub fn empty(format: EngineStateFormat) -> Self {
        Self::of(format, Vec::new())
    }

    /// A state of `bytes` in `format`, standing on no heap.
    #[must_use]
    pub fn of(format: EngineStateFormat, bytes: Vec<u8>) -> Self {
        Self {
            staged: Vec::new(),
            format,
            bytes,
            pages: None,
            requirements: ClaimRequirements::default(),
        }
    }
}

/// A heap root as JSON text: its lowercase hex.
mod page_hex {
    use lash_core_store::checkpoint_graph::NodeId;
    use serde::{Deserialize as _, Deserializer, Serializer};

    #[expect(clippy::ref_option, reason = "serde's `with` signature")]
    pub(super) fn serialize<S: Serializer>(
        page: &Option<NodeId>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match page {
            Some(page) => serializer.serialize_some(&page.to_hex()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<NodeId>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| {
                NodeId::from_hex(&text)
                    .ok_or_else(|| serde::de::Error::custom("a heap root is 64 lowercase hex"))
            })
            .transpose()
    }
}

/// A read-only status an engine projects from its committed state: the
/// engine's own typed data, in the format of the state it was projected
/// from. Lash records it beside the process's driver in each transition's
/// own commit and never interprets it; a host reads the last committed one
/// back ([`EngineStatusProjection`]). It is a read, not a lifecycle fact.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineStatus {
    /// The format of the state the status was projected from.
    pub format: EngineStateFormat,
    /// The engine's typed status, as its own data.
    pub status: serde_json::Value,
}

/// An engine's projection of a host-readable status from its state,
/// declared at registration
/// ([`ProcessEngineRegistration::with_status_projection`](super::ProcessEngineRegistration::with_status_projection)).
/// Lash calls it with the state each transition commits and records the
/// answer in the same commit, so the status never disagrees with the state
/// it was projected from. An engine registered without one has no status.
pub trait EngineStatusProjection: Send + Sync {
    /// The status `state` projects, or `None` for a state with none:
    /// synchronous and effect-free, like `advance`.
    fn status(&self, state: &EngineState) -> Option<serde_json::Value>;
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
    pub fn site(&self) -> Option<&lash_vocabulary::sites::EffectId> {
        match self {
            Self::Tool { site, .. } => site.as_deref().map(|site| &site.at),
            Self::Engine { .. } => None,
        }
    }

    /// The performing identity and authored origin supplied by the engine.
    pub fn location(&self) -> Option<&lash_vocabulary::sites::EffectLocation> {
        match self {
            Self::Tool { site, .. } => site.as_deref(),
            Self::Engine { .. } => None,
        }
    }

    /// The request's internal work identity: an operation use's link and
    /// slot, or the engine body's kind. Host-facing effect events use the
    /// qualified operation name retained at admission.
    #[must_use]
    pub fn operation(&self) -> String {
        match self {
            Self::Tool { use_, .. } => format!("{}:{}", use_.link, use_.slot.0),
            Self::Engine { kind, .. } => kind.0.clone(),
        }
    }
}

impl EngineStepKind {
    /// The tool an engine step's admission records for a process of engine
    /// `engine`: `<engine>/<kind>`, which only identifies the record. An
    /// engine step is dispatched by its variant, never by this name.
    #[must_use]
    pub fn admitted_tool(&self, engine: &str) -> lash_vocabulary::tool_contract::ToolId {
        lash_vocabulary::tool_contract::ToolId::new(format!("{engine}/{}", self.0))
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

/// Answers a process's read-mode operations from its recorded environment.
/// Reads admit no step and are repeated when an uncommitted run is retried.
#[async_trait::async_trait]
pub trait EngineEffectReads: Send + Sync {
    /// Read the route pinned by `effect` and `contract`. The caller supplies
    /// plain tool input (or its projection refusal) and its parent deadline; the route also applies the
    /// tool's execution bound and the process cancellation.
    ///
    /// # Errors
    /// An unavailable reader or codec faults the run. A tool's own refusal
    /// stays a typed, catchable read error.
    async fn read(
        &self,
        use_: &lash_vocabulary::names::OperationUse,
        input: Result<serde_json::Value, String>,
        cancel: &tokio_util::sync::CancellationToken,
        deadline: std::time::Instant,
    ) -> Result<Result<serde_json::Value, EngineReadError>, crate::plugin_error::PluginError>;
}

/// A catchable refusal of an inline process read.
#[derive(Debug)]
pub enum EngineReadError {
    /// The guest operands cannot be projected into one tool input record.
    Arguments(String),
    /// The pinned contract or input does not fit the tool.
    Tool(Box<crate::ToolFailure>),
    /// The reader's own error, with its original data.
    Reader {
        message: String,
        data: serde_json::Value,
    },
    /// The process stopped during the read.
    Cancelled,
    /// The tool or parent deadline elapsed.
    TimedOut,
}

/// What an engine step reads and stages heap pages under: the store's page
/// publication and the process actor's fence, whose leases its reads hold.
#[derive(Clone)]
pub struct StepPages {
    pub store: std::sync::Arc<dyn lash_durable::domain::pages::PageStore>,
    pub actor: lash_durable::ActorKey,
    pub epoch: lash_durable::Epoch,
}

impl std::fmt::Debug for StepPages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepPages")
            .field("actor", &self.actor)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

/// The heap pages an engine step's outcome publishes (FIG-6193, P1): a
/// CheckpointRoot the body staged, which the step's outcome record names
/// and owns. The lifecycle publishes it with the outcome's run fact in one
/// fenced transaction, owned by that fact
/// ([`CandidateOwner::RunFactAppend`](lash_durable::domain::pages::CandidateOwner::RunFactAppend)),
/// and settles or abandons the staging after.
#[async_trait::async_trait]
pub trait StepPublication: Send + Sync {
    /// The CheckpointRoot page the outcome names.
    fn root(&self) -> lash_core_store::checkpoint_graph::NodeId;

    /// The candidate rooted at [`root`](Self::root), with no base,
    /// published under `commit`, and that root page. What the pages name
    /// beyond the heap and the store may not hold yet, such as the release
    /// the root pins, is staged in the outcome's `tx`.
    ///
    /// # Errors
    ///
    /// The staging's refusal: nothing is published, and the outcome is not
    /// recorded.
    async fn publication(
        &self,
        tx: &mut lash_durable::ActorTx,
        commit: lash_core_store::checkpoint_graph::CommitId,
    ) -> Result<StepManifest, lash_durable::DurableError>;

    /// The publication under `commit` committed: the pages are the
    /// store's, and the staging lets them go.
    async fn settled(&self, commit: lash_core_store::checkpoint_graph::CommitId);

    /// The publication under `commit` was refused before it entered the
    /// store: the staging keeps its pages, and the outcome publishes them
    /// again under a new commit.
    async fn withdrawn(&self, commit: lash_core_store::checkpoint_graph::CommitId);

    /// Nothing was published: the staging is dropped, and none of its
    /// pages is ever reachable.
    async fn abandoned(&self);
}

/// What a [`StepPublication`] publishes under one commit: the candidate's
/// manifest, and its root, the CheckpointRoot page the outcome's record
/// names. Only the manifest supplies that page, not the outcome's staged
/// nodes, so the facts the commit appends decode only with it.
pub struct StepManifest {
    pub manifest: Box<dyn lash_core_store::checkpoint_graph::stage::CandidateManifest>,
    pub root: lash_core_store::checkpoint_graph::EncodedNode,
}

/// An engine step outcome's [`StepPublication`], compared by its root.
#[derive(Clone)]
pub struct StepCandidate(pub std::sync::Arc<dyn StepPublication>);

impl StepCandidate {
    /// The CheckpointRoot the outcome names.
    #[must_use]
    pub fn root(&self) -> lash_core_store::checkpoint_graph::NodeId {
        self.0.root()
    }
}

impl std::fmt::Debug for StepCandidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StepCandidate").field(&self.root()).finish()
    }
}

impl PartialEq for StepCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.root() == other.root()
    }
}

impl Eq for StepCandidate {}

/// Which host capacity an engine step's attempt did not get: the closed set
/// of verdicts a [`CapacityVerdict`] names. Each is the host's, read live,
/// and a replay, a later attempt or another node with capacity decides it
/// otherwise (FIG-4451); none is a fault of the step's input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CapacityCause {
    /// The host's worker queue refused the attempt at its item or byte bound.
    QueueFull,
    /// No worker came free within the host's checkout wait.
    CheckoutTimedOut,
    /// Repeated worker failures exhausted the host's restart window.
    RestartStorm,
    /// Retryable worker infrastructure failed the attempt: a worker that
    /// crashed before it computed the run, an unresponsive or
    /// protocol-breaking worker, or a deadline or budget of the host's
    /// rather than of the run. A worker that died running the run is a
    /// fault of the attempt instead, which spends one (FIG-6786).
    RetryableInfrastructure,
}

impl std::fmt::Display for CapacityCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::QueueFull => "the worker queue is full",
            Self::CheckoutTimedOut => "no worker came free within the checkout wait",
            Self::RestartStorm => "worker failures exhausted the restart window",
            Self::RetryableInfrastructure => "retryable worker infrastructure failed",
        })
    }
}

/// An engine step's attempt the host lacked capacity for (FIG-6155): the
/// attempt has no outcome, and nothing of it is recorded. The lifecycle
/// keeps the step's start, the process's activation backs off for
/// `retry_after`, doubled for each activation in a row that made no
/// progress, and ends without committing; the next claim runs the step
/// again at its same ordinal, uncounted, as after a crash. A process whose
/// claims keep meeting the verdict parks with
/// [`ProcessParkReason::ActivationLoop`](crate::ProcessParkReason::ActivationLoop)
/// once the activation-loop budget is reached; a redrive runs it again. A
/// capacity verdict never ends a process.
///
/// **Soundness.** Recording nothing is sound only when the attempt produced
/// no effect anyone outside it could see: the verdict came before the body
/// started its work (a full queue, a checkout that timed out), or the body
/// is a recomputation from committed state, which its next run repeats. A
/// body answers it only then. The lifecycle honours it only from a step whose
/// pinned policy is `Repeatable` and that the current declaration still
/// repeats; from any other step it records `Interrupted`, the outcome of
/// an attempt that started and never settled.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{cause}: {detail}")]
pub struct CapacityVerdict {
    /// Which capacity the attempt did not get.
    pub cause: CapacityCause,
    /// How long the activation waits before it ends, before doubling.
    pub retry_after: Duration,
    /// The host's own account of the verdict, for diagnostics.
    pub detail: String,
}

impl CapacityVerdict {
    /// The verdict `cause`, retried after `retry_after`, with `detail`.
    #[must_use]
    pub fn new(cause: CapacityCause, retry_after: Duration, detail: impl Into<String>) -> Self {
        Self {
            cause,
            retry_after,
            detail: detail.into(),
        }
    }
}

/// What one engine step's body answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineStepAnswer {
    /// Its outcome, recorded as the attempt's.
    Settled(Box<SettledOutput>),
    /// Its completion, recorded as the attempt's with the heap pages it
    /// publishes: the outcome's run fact names `candidate`'s root and owns
    /// it from the commit that records both.
    Paged {
        output: Box<SettledOutput>,
        candidate: StepCandidate,
    },
    /// The host lacked capacity for the attempt; see [`CapacityVerdict`].
    Capacity(CapacityVerdict),
}

impl From<SettledOutput> for EngineStepAnswer {
    fn from(output: SettledOutput) -> Self {
        Self::Settled(Box::new(output))
    }
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

/// A timer an engine waits for while its steps run, with the workflow
/// occurrence whose sleep owns it. Engines without a document name no site.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineWake {
    pub until: DurableInstant,
    pub site: Option<lash_vocabulary::sites::EffectId>,
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
        wake: Option<EngineWake>,
    },
    /// Pin a host-resolvable key, minted before any step submits it.
    PinKey {
        /// The wait's name.
        name: KeyName,
        /// How long the wait may stay open: the engine's own bound, with no
        /// lash default or ceiling.
        bound: lash_vocabulary::tool_contract::ParkBound,
    },
    /// Wait for a pinned key's resolution.
    AwaitExternal {
        /// The wait's name.
        name: KeyName,
        /// The node that waits, when the engine's workflow document names one.
        site: Option<lash_vocabulary::sites::EffectId>,
    },
    /// Wait for another process's terminal.
    AwaitProcess {
        /// The awaited process.
        process: ProcessId,
        /// How long the wait may stay open: the engine's own bound, with no
        /// lash default or ceiling.
        bound: lash_vocabulary::tool_contract::ParkBound,
        /// The node that waits, when the engine's workflow document names one.
        site: Option<lash_vocabulary::sites::EffectId>,
    },
    /// Sleep until a durable instant.
    Sleep {
        /// When the process wakes.
        until: DurableInstant,
        /// The node that sleeps, when the engine's workflow document names one.
        site: Option<lash_vocabulary::sites::EffectId>,
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
        origin: lash_protocol_types::CancelOrigin,
        /// When lash forces the terminal.
        grace_until: DurableInstant,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineState {
    /// Private nodes a carry transfers to the owner transaction with this state.
    #[serde(skip)]
    pub staged: Vec<lash_core_store::checkpoint_graph::EncodedNode>,
    /// The encoding of `bytes`.
    pub format: EngineStateFormat,
    /// The engine's own encoding of its state.
    pub bytes: Vec<u8>,
    /// The persistent-heap root the state stands on: the CheckpointRoot
    /// page a step's outcome published ([`EngineStepAnswer::Paged`]), which
    /// the process's execution root owns from the advance that names it
    /// (FIG-6193). None for an engine that keeps no heap.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "page_hex")]
    pub pages: Option<lash_core_store::checkpoint_graph::NodeId>,
    /// What running the process requires of a node while it stands on
    /// this state (FIG-6985): the natives and kernel versions its parked
    /// run's code reaches and the providers and codecs of its env's
    /// links. Its execution root records it; empty when no run is parked.
    pub requirements: ClaimRequirements,
}

use crate::ProjectionProviders;
use crate::process::EngineSettings;
use lash_core_ids::clock::Clock;
use lash_core_store::tool_surface::ExecutionEnvironment;

use lash_vocabulary::{ExecutionPolicy, claims::*, names::*};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StepRequest {
    Tool {
        step: StepName,
        use_: OperationUse,
        input: serde_json::Value,
        site: Option<Box<lash_vocabulary::sites::EffectLocation>>,
    },
    Engine {
        step: StepName,
        kind: EngineStepKind,
        input: serde_json::Value,
    },
}
#[derive(Clone)]
pub struct EngineStepRun {
    pub process: ProcessId,
    pub execution: ExecutionId,
    pub engine_settings: Option<EngineSettings>,
    pub environment: std::sync::Arc<ExecutionEnvironment>,
    /// Registered readers over the recorded per-use environment.
    pub reads: std::sync::Arc<dyn EngineEffectReads>,
    pub resolutions: std::sync::Arc<BTreeMap<OperationUse, ResolutionEntry>>,
    pub now: DurableInstant,
    pub clock: std::sync::Arc<dyn Clock>,
    pub projection_providers: Option<std::sync::Arc<dyn ProjectionProviders>>,
    pub kind: EngineStepKind,
    pub input: serde_json::Value,
    pub pages: Option<StepPages>,
}
#[async_trait::async_trait]
/// Neutral admitted engine bodies, per-use environment and committed resolutions. Core derives route/contract itself.
pub trait EngineSteps: Send + Sync {
    fn kinds(&self) -> Vec<EngineStepKind>;
    fn execution(&self, kind: &EngineStepKind) -> std::time::Duration;
    fn retry(&self, kind: &EngineStepKind) -> ExecutionPolicy;
    async fn run(
        &self,
        run: EngineStepRun,
        cancel: tokio_util::sync::CancellationToken,
    ) -> EngineStepAnswer;
}
