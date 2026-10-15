//! A kernel process's engine state: its saved run and the waits that run
//! stands on.
//!
//! The machine parks; the engine commits (kernel spec §9 rule 4). The state
//! holds the run as the machine exported it at its last park, as sealed
//! checkpoint graph state (a KernelWhole), the admission of every wait the
//! run holds (the state holds a wait as its identity alone), every wait
//! that park left open with the step or timer that answers it, and the
//! outcomes that settled while a run was in flight. It is committed with
//! the admission of the steps a park asked for, in one `process.advance`
//! transaction.

use std::collections::BTreeMap;

use lash_core::StepName;
use lash_kernel_doc::{Datum, DocumentId, EffectIdentity, Name};
use lash_kernel_state::AdmittedWait;
use lash_vm_client::wire::OutcomeWire;
use lash_vm_protocol::{PayloadKind, SealedGraphState};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// A parked run as the engine's JSON state carries it: its sealed graph
/// state in the wire encoding, as base64 text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SealedRun(pub(crate) SealedGraphState);

impl Serialize for SealedRun {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let payload = lash_vm_client::wire::encode(PayloadKind::ParkedRun, &self.0)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&BASE64.encode(payload.0))
    }
}

impl<'de> Deserialize<'de> for SealedRun {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let bytes = BASE64.decode(text).map_err(serde::de::Error::custom)?;
        lash_vm_client::wire::decode(
            PayloadKind::ParkedRun,
            &lash_vm_protocol::EncodedPayload(bytes),
        )
        .map(Self)
        .map_err(serde::de::Error::custom)
    }
}

/// The admission of one wait the run holds: the request the state does not
/// carry, in the kernel's binary encoding, as base64 text.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Admission(pub(crate) AdmittedWait);

impl Admission {
    pub(crate) fn identity(&self) -> &EffectIdentity {
        &self.0.identity
    }
}

impl Serialize for Admission {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer
            .serialize_str(&BASE64.encode(lash_kernel_doc::graph_tokens::encode_body(&self.0)))
    }
}

impl<'de> Deserialize<'de> for Admission {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let bytes = BASE64.decode(text).map_err(serde::de::Error::custom)?;
        lash_kernel_doc::graph_tokens::decode_body(&bytes)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

use base64::Engine as _;
const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

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
    pub(crate) parked: Option<SealedRun>,
    /// The admission of every wait the parked run holds, by the machine's
    /// wait id, until a committed run has been handed its outcome.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) admissions: BTreeMap<u64, Admission>,
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
    pub(crate) parked: Option<SealedRun>,
    /// The admission of each wait the parked run holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) admissions: Vec<Admission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deliver: Vec<Delivery>,
}

/// One wait the machine asked for at a park, as the step resolved it
/// against the host boundary, with its admission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Issued {
    /// A `perform` of an effect the process's env links: its path, which
    /// the link names the tool of (P3), and the tool's input.
    Effect {
        wait: u64,
        admission: Admission,
        effect: lash_kernel_doc::EffectName,
        input: serde_json::Value,
    },
    /// A `perform` the boundary refuses: the error is raised at the
    /// `perform`, which the guest may catch, and nothing is dispatched.
    Refused {
        wait: u64,
        admission: Admission,
        kind: String,
        message: String,
    },
    /// A `sleep`.
    Sleep {
        wait: u64,
        admission: Admission,
        until_ms: i64,
    },
}

impl Issued {
    pub(crate) fn wait(&self) -> u64 {
        match self {
            Self::Effect { wait, .. } | Self::Refused { wait, .. } | Self::Sleep { wait, .. } => {
                *wait
            }
        }
    }

    pub(crate) fn admission(&self) -> &Admission {
        match self {
            Self::Effect { admission, .. }
            | Self::Refused { admission, .. }
            | Self::Sleep { admission, .. } => admission,
        }
    }
}

/// What one `kernel_run` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum KernelRunOutput {
    /// The run parked: its exported state, the waits it asked for since
    /// its last park and the waits it withdrew.
    Parked {
        state: SealedRun,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        issued: Vec<Issued>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        withdrawn: Vec<u64>,
    },
    /// The run ended.
    Ended {
        #[serde(serialize_with = "lash_sansio::durable_serde::serialize")]
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
