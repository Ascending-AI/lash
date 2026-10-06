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

/// One step an engine asks lash to run: a catalog tool, under its
/// declaration, `ExecutionPolicy` and ceiling. Its identity is
/// `(process, step, ordinal)`, and its `ToolCallId` derives from it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StepRequest {
    /// The step's name.
    pub step: StepName,
    /// The catalog tool it runs.
    pub tool: lash_sansio::ToolId,
    /// The tool's input.
    pub input: serde_json::Value,
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
    /// End the process.
    Terminal(ProcessOutcome),
}

/// What happened to a process since its last transition.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "the pinned event shape (S6): one event per transition, handed to advance by value"
)]
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
        /// How its attempt ended.
        outcome: lash_core_store::tool_run::AttemptOutcome,
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
