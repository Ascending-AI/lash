//! A kernel process's engine state: its saved run and the waits that run
//! stands on.
//!
//! The machine parks; the engine commits (kernel spec §9 rule 4). The state
//! holds the run as the machine exported it at its last park, every wait
//! that park left open with the step or timer that answers it, and the
//! outcomes that settled while a run was in flight. It is committed with
//! the admission of the steps a park asked for, in one `process.advance`
//! transaction.

use std::collections::BTreeMap;

use lash_core::StepName;
use lash_kernel_doc::{Datum, DocumentId, EffectIdentity, Name};
use lash_sansio::ToolId;
use lash_vm_client::OpaqueVmState;
use lash_vm_client::wire::OutcomeWire;
use serde::{Deserialize, Serialize};

/// The engine step that runs the machine from its saved state to its next
/// park.
pub const KERNEL_RUN_STEP: &str = "kernel_run";

/// What a process of this engine starts from: the admitted document it
/// runs, the entry it enters and the entry's arguments by parameter name.
///
/// The document is named by identity, so a running instance keeps the
/// document it was admitted under whatever is published after it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelProcessInput {
    pub document: DocumentId,
    pub entry: Name,
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
}

impl KernelProcessInput {
    /// The input a start payload or a definition value states.
    ///
    /// # Errors
    ///
    /// The decode error of a payload that is not this engine's.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(payload.clone())
    }

    /// The definition this input is a start of: the document and the entry,
    /// without arguments.
    pub fn definition(&self) -> KernelProcessDefinition {
        KernelProcessDefinition {
            document: self.document,
            entry: self.entry.clone(),
        }
    }
}

/// The value of one immutable definition: an entry of an admitted document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelProcessDefinition {
    pub document: DocumentId,
    pub entry: Name,
}

impl KernelProcessDefinition {
    /// The definition a start payload or a definition value names.
    ///
    /// # Errors
    ///
    /// The decode error of a payload that is not this engine's.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, serde_json::Error> {
        // A start payload carries the arguments beside the definition.
        KernelProcessInput::from_payload(payload).map(|input| input.definition())
    }

    /// The executable generation a process of the definition runs as: the
    /// kernel version this build's workers run, the document and the
    /// entry. A process started under another kernel version is another
    /// generation.
    pub fn executable_generation(&self) -> lash_core::ExecutableGeneration {
        lash_core::ExecutableGeneration::new(format!(
            "kernel:{}:{}:{}",
            crate::LASH_KERNEL_VERSION,
            self.document,
            self.entry
        ))
    }

    /// The store key of the document the definition runs.
    pub fn artifact_ref(&self) -> String {
        self.document.to_string()
    }
}

/// One outcome the machine has not been handed in a committed state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delivery {
    pub(crate) wait: u64,
    /// Which wait of the run this is, for the fact its delivery is
    /// observed as.
    pub(crate) identity: EffectIdentity,
    pub(crate) outcome: OutcomeWire,
}

/// What answers one open wait of the saved run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Wait {
    /// A `perform`: the step that runs its effect.
    Step {
        step: StepName,
        identity: EffectIdentity,
    },
    /// A `sleep`: the instant it ends.
    Timer {
        until_ms: i64,
        identity: EffectIdentity,
    },
}

/// Where the process is between transitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Phase {
    /// A `kernel_run` is admitted: it resumes the saved run and hands it
    /// `delivering`.
    Running {
        step: StepName,
    },
    /// The run is parked on its open waits.
    Parked,
    Ended,
}

/// The engine state of one process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KernelEngineState {
    pub(crate) payload: serde_json::Value,
    /// The run as the machine exported it at its last park; `None` before
    /// the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parked: Option<OpaqueVmState>,
    /// How many `kernel_run` steps the process has asked for.
    pub(crate) runs: u64,
    /// How many effect steps the process has asked for.
    pub(crate) effects: u64,
    /// The open waits of the saved run, by the machine's wait id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) waits: BTreeMap<u64, Wait>,
    /// Outcomes that settled and no committed run has consumed, in the
    /// order they settled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) settled: Vec<Delivery>,
    pub(crate) phase: Phase,
}

/// The input of one `kernel_run`: everything the step recomputes from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KernelRunInput {
    pub(crate) payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parked: Option<OpaqueVmState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deliver: Vec<Delivery>,
}

/// One wait the machine asked for at a park, as the step resolved it
/// against the host boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Issued {
    /// A `perform` of an effect the host offers: the tool that runs it and
    /// the tool's input.
    Effect {
        wait: u64,
        identity: EffectIdentity,
        tool: ToolId,
        input: serde_json::Value,
    },
    /// A `perform` the boundary refuses: the error is raised at the
    /// `perform`, which the guest may catch, and nothing is dispatched.
    Refused {
        wait: u64,
        identity: EffectIdentity,
        kind: String,
        message: String,
    },
    /// A `sleep`.
    Sleep {
        wait: u64,
        identity: EffectIdentity,
        until_ms: i64,
    },
}

/// What one `kernel_run` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum KernelRunOutput {
    /// The run parked: its exported state, the waits it asked for since
    /// its last park and the waits it withdrew.
    Parked {
        state: OpaqueVmState,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        issued: Vec<Issued>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        withdrawn: Vec<u64>,
    },
    /// The run ended.
    Ended {
        outcome: Box<lash_core::ProcessOutcome>,
    },
}

/// A refused effect's error, as the machine is handed it.
pub(crate) fn refused(kind: &str, message: String) -> OutcomeWire {
    OutcomeWire::Failed(lash_kernel_doc::ErrorDatum {
        kind: kind.to_owned(),
        message,
        data: Datum::Null,
    })
}
