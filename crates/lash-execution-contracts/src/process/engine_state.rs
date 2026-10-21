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

use crate::waits::PinnedKey;
use crate::{ProcessId, ProcessOutcome, Resolution};
use lash_core_store::tool_run::SettledOutput;

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
    #[serde(
        default,
        skip_serializing_if = "lash_kernel_doc::ExecutionRequirements::is_empty"
    )]
    pub requirements: lash_kernel_doc::ExecutionRequirements,
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
            requirements: lash_kernel_doc::ExecutionRequirements::default(),
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

    /// `state`, written in a carried format, in the engine's own; or
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
    /// The kernel migration's typed refusal.
    pub refusal: lash_kernel_migrate::KernelMigrationRefusal,
    /// The reason in words.
    pub message: String,
    /// The same state will be refused again: the process is parked with
    /// the refusal. A refusal that is not final, such as a store that did
    /// not answer, is asked again at the next claim.
    pub fatal: bool,
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

/// One step an engine asks lash to run. Its identity is
/// `(process, step, ordinal)`, and its `ToolCallId` derives from it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StepRequest {
    /// A linked call, under its tool's declaration, `ExecutionPolicy` and
    /// ceiling.
    Tool {
        /// The step's name.
        step: StepName,
        /// The effect it performs, by its path (P3): the process env's link
        /// of it names the tool that runs, never the engine.
        effect: lash_kernel_doc::EffectName,
        /// The contract the performing code requires of the effect
        /// (OP-CONTRACT-PIN): the call runs only on a tool serving it.
        contract: lash_kernel_doc::ContractId,
        /// The tool's input.
        input: serde_json::Value,
        /// The site of the engine's workflow document it runs for, when the
        /// engine names one: its committed outcome is recorded as that
        /// site's `process.effect_outcome`, and the start of its admitted
        /// body is observed there ([`lash_trace::StepBodyStarted`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<Box<lash_protocol_types::EffectId>>,
        /// The recorded match a granted tool's call runs on (P6): set only
        /// by lash, on the step it issues once the call's resolution
        /// committed, which runs the matched tool with the arguments the
        /// grant's transform names for `input`. An engine never sets it: it
        /// asks for the granted tool's effect, whose step is the resolution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        matched: Option<Box<crate::DeferredMatch>>,
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

    /// The recorded match the step runs on, when it is the target of a
    /// granted tool's call.
    #[must_use]
    pub fn matched(&self) -> Option<&crate::DeferredMatch> {
        match self {
            Self::Tool { matched, .. } => matched.as_deref(),
            Self::Engine { .. } => None,
        }
    }

    /// The effect node the step runs for, if its engine named one.
    #[must_use]
    pub fn site(&self) -> Option<&lash_protocol_types::EffectId> {
        match self {
            Self::Tool { site, .. } => site.as_deref(),
            Self::Engine { .. } => None,
        }
    }

    /// What its process's operations name the step's work: the effect's
    /// path, or the engine body's kind.
    #[must_use]
    pub fn operation(&self) -> String {
        match self {
            Self::Tool { effect, .. } => effect.to_string(),
            Self::Engine { kind, .. } => kind.0.clone(),
        }
    }
}

impl EngineStepKind {
    /// The tool an engine step's admission records for a process of engine
    /// `engine`: `<engine>/<kind>`, which only identifies the record. An
    /// engine step is dispatched by its variant, never by this name.
    #[must_use]
    pub fn admitted_tool(&self, engine: &str) -> lash_protocol_types::ToolId {
        lash_protocol_types::ToolId::new(format!("{engine}/{}", self.0))
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
/// recorded, the effects its env links, the activation's clock reading and
/// the step's own kind and input.
#[derive(Clone)]
pub struct EngineStepRun {
    /// The process the step belongs to.
    pub process: ProcessId,
    /// The engine settings the process's start stated and its row records
    /// (D4): never this build's defaults.
    pub engine_settings: Option<super::EngineSettings>,
    /// The effects the process's env links (P3): what its linked calls
    /// may perform, each by its link.
    pub effects: std::sync::Arc<
        std::collections::BTreeMap<lash_kernel_doc::EffectName, crate::OperationLink>,
    >,
    /// The activation's clock reading when the body started.
    pub now: DurableInstant,
    /// The activation's clock, for a body that waits.
    pub clock: std::sync::Arc<dyn crate::Clock>,
    /// The backend's projection providers, which a body that runs a VM reads
    /// projections through (ADR 0132 §9).
    pub projection_providers: Option<std::sync::Arc<dyn crate::projection::ProjectionProviders>>,
    /// Which body runs.
    pub kind: EngineStepKind,
    /// The body's input.
    pub input: serde_json::Value,
    /// The activation's heap page publication, for a body whose outcome
    /// publishes pages ([`EngineStepAnswer::Paged`]); None on a store that
    /// holds no heap pages.
    pub pages: Option<StepPages>,
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

/// An engine's own step bodies, declared at registration
/// ([`ProcessEngineRegistration::with_engine_steps`](super::ProcessEngineRegistration::with_engine_steps)).
/// Each runs under the retry policy its kind declares, pinned at admission:
/// under `Repeatable`, a crash before its outcome commits runs it again from
/// the same input and a failure is retried within the policy's attempts, so
/// such a body must be a recomputation from that input, with no effect of
/// its own; under `Once`, a crash settles it `Interrupted` and a failure is
/// its outcome. A body that the host lacked capacity for answers a
/// [`CapacityVerdict`] instead of a failure: under `Repeatable` it runs
/// again uncounted and records nothing, so its attempts are spent only on
/// failures of its own.
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
    fn retry(&self, kind: &EngineStepKind) -> lash_protocol_types::ExecutionPolicy;

    /// Run one body to its outcome, observing `cancel`: its settled output,
    /// or a [`CapacityVerdict`] when the host lacked capacity for it.
    async fn run(
        &self,
        run: EngineStepRun,
        cancel: tokio_util::sync::CancellationToken,
    ) -> EngineStepAnswer;
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
    pub site: Option<lash_protocol_types::EffectId>,
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
        bound: crate::ParkBound,
    },
    /// Wait for a pinned key's resolution.
    AwaitExternal {
        /// The wait's name.
        name: KeyName,
        /// The node that waits, when the engine's workflow document names one.
        site: Option<lash_protocol_types::EffectId>,
    },
    /// Wait for another process's terminal.
    AwaitProcess {
        /// The awaited process.
        process: ProcessId,
        /// How long the wait may stay open: the engine's own bound, with no
        /// lash default or ceiling.
        bound: crate::ParkBound,
        /// The node that waits, when the engine's workflow document names one.
        site: Option<lash_protocol_types::EffectId>,
    },
    /// Sleep until a durable instant.
    Sleep {
        /// When the process wakes.
        until: DurableInstant,
        /// The node that sleeps, when the engine's workflow document names one.
        site: Option<lash_protocol_types::EffectId>,
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
