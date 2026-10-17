//! A kernel process's engine state: its saved run and the waits that run
//! stands on.
//!
//! The machine parks; the engine commits (kernel spec §9 rule 4). The state
//! names the run as the machine sealed it at its last park, by its
//! persistent-heap CheckpointRoot page (FIG-6193), the admission of every
//! wait the run holds (the state holds a wait as its identity alone), every
//! wait that park left open with the step or timer that answers it, and the
//! outcomes that settled while a run was in flight. It is committed with
//! the admission of the steps a park asked for, in one `process.advance`
//! transaction, whose execution root owns the page from then on.

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::NodeId;
use lash_execution_contracts::process::StepName;
use lash_kernel_doc::{Datum, DocumentId, EffectIdentity, FunctionId, Name};
use lash_kernel_state::AdmittedWait;
use lash_vm_client::wire::OutcomeWire;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The engine step that runs the machine from its saved state to its next
/// park.
pub const KERNEL_RUN_STEP: &str = "kernel_run";

/// What a process of this engine starts from: the admitted document it
/// runs, the library functions its `use` lines name, the entry it enters
/// and the entry's arguments by parameter name.
///
/// The document is named by identity, so a running instance keeps the
/// document it was admitted under whatever is published after it. The
/// functions are the document's own manifest, stated beside its identity so
/// that what a start holds is read from the start alone (FIG-6379): every
/// read checks them against the document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelProcessInput {
    pub document: DocumentId,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub functions: BTreeSet<FunctionId>,
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
            functions: self.functions.clone(),
            entry: self.entry.clone(),
        }
    }
}

/// The value of one immutable definition: an entry of an admitted document,
/// with the library functions the document lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelProcessDefinition {
    pub document: DocumentId,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub functions: BTreeSet<FunctionId>,
    pub entry: Name,
}

impl KernelProcessDefinition {
    /// The definition of `entry` of `document`, whose identity is
    /// `identity`: pure, from the document alone.
    pub fn of(document: &lash_kernel_doc::Document, identity: DocumentId, entry: Name) -> Self {
        Self {
            document: identity,
            functions: document.manifest.functions.keys().copied().collect(),
            entry,
        }
    }

    /// A start of this definition with `args`.
    pub fn start(&self, args: serde_json::Map<String, serde_json::Value>) -> KernelProcessInput {
        KernelProcessInput {
            document: self.document,
            functions: self.functions.clone(),
            entry: self.entry.clone(),
            args,
        }
    }

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

    /// What a definition and every process of it hold: the document it
    /// runs, and each library function it lists, by identity.
    pub fn artifacts(&self) -> Vec<lash_core::ArtifactName> {
        std::iter::once(super::documents::document_artifact(&self.document))
            .chain(
                self.functions
                    .iter()
                    .map(super::documents::function_artifact),
            )
            .collect()
    }
}

/// A parked run as the engine's state names it: the CheckpointRoot page a
/// `kernel_run` published with its outcome, and the kernel version its
/// header states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParkedPage {
    #[serde(with = "page_hex")]
    pub(crate) root: NodeId,
    pub(crate) kernel: u32,
}

/// A page's identity as JSON text: its lowercase hex.
mod page_hex {
    use super::{Deserialize as _, Deserializer, NodeId, Serializer};

    pub(super) fn serialize<S: Serializer>(
        page: &NodeId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&page.to_hex())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<NodeId, D::Error> {
        let text = String::deserialize(deserializer)?;
        NodeId::from_hex(&text)
            .ok_or_else(|| serde::de::Error::custom("a page identity is 64 lowercase hex"))
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
    /// Which retained record the outcome is read from: what a replay reads
    /// it back from instead of a copy (FIG-6370).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<DeliverySource>,
}

/// The retained record an outcome the machine is handed is read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DeliverySource {
    /// The settled output of the effect step `step`, in the run journal.
    Step { step: StepName },
    /// A sleep's timer: the outcome is always `Elapsed`.
    Elapsed,
    /// A `perform` the boundary refused, as `kernel_run.{run}` issued it.
    Refused { run: u64 },
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
    /// The run as the machine sealed it at its last park; `None` before
    /// the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parked: Option<ParkedPage>,
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
    /// Which `kernel_run` answered `parked` with its output, when one did:
    /// `None` before the first park and once a carry rewrote the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parked_by: Option<u64>,
    /// Which `kernel_run` issued each wait in `admissions`, while its
    /// admission is the one that output holds: a carry that rewrote the
    /// admissions forgets them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) issued_by: BTreeMap<u64, u64>,
}

/// The input of one `kernel_run`: everything the step recomputes from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KernelRunInput {
    pub(crate) payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parked: Option<ParkedPage>,
    /// The admission of each wait the parked run holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) admissions: Vec<Admission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deliver: Vec<Delivery>,
    /// Which `kernel_run` answered `parked` with its output, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parked_by: Option<u64>,
    /// For each of `admissions`, in order: its wait and the `kernel_run`
    /// whose output holds it as issued, when one does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) admitted_by: Vec<Option<(u64, u64)>>,
}

/// What the host's reads answered one run segment, recorded at
/// `observe: counts` (FIG-6370): the clock instant it read, at most one
/// since the segment's clock is constant, and each random draw in order. A
/// replay answers the same reads from it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostReads {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) now_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) random: Vec<u64>,
}

/// Where one run segment started, recorded at `observe: counts` beside its
/// end (FIG-6370). The step's input is not journaled, so this is what a
/// replay re-runs the segment from: the document, the state it resumed,
/// and the admissions and outcomes it was handed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordedStart {
    pub(crate) document: DocumentId,
    pub(crate) from: StartedFrom,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) admissions: Vec<RecordedAdmission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deliver: Vec<RecordedDelivery>,
}

/// One admission a recorded segment was handed: by reference to the output
/// that issued it, or whole when a carry rewrote it and no record holds it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RecordedAdmission {
    Issued { wait: u64, run: u64 },
    Whole(Box<Admission>),
}

/// One outcome a recorded segment was handed: by reference to the record it
/// is read from, with the digest of the outcome as the machine got it, or
/// whole when no record names its source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RecordedDelivery {
    Of {
        wait: u64,
        source: DeliverySource,
        digest: String,
    },
    Whole(Box<Delivery>),
}

/// The digest a [`RecordedDelivery::Of`] binds its outcome by.
pub(crate) fn outcome_digest(outcome: &OutcomeWire) -> String {
    lash_core_ids::stable_hash::sha256_hex(&serde_json::to_vec(outcome).unwrap_or_default())
}

/// The state a recorded segment resumed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StartedFrom {
    /// The document's entry: the process's first segment.
    Entry,
    /// The page `kernel_run.{run}` parked with, whose root is `root`
    /// (hex): that output's record owns it while the journal retains it,
    /// and a replay reads it under that record.
    Output { run: u64, root: String },
}

/// What a segment recorded for observation at `observe: counts`: its
/// statement-start delta, its host reads and its start. Every part is empty
/// at `off`, and in an output recorded before they existed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Observed<'a> {
    /// The segment's statement starts and empty-block entries, by document
    /// site ordinal.
    pub(crate) ran: &'a lash_kernel_vm::RanDelta,
    pub(crate) reads: Option<&'a HostReads>,
    pub(crate) start: Option<&'a RecordedStart>,
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
    /// The run parked: the page it sealed, which the step's outcome
    /// published and owns, the waits it asked for since its last park and
    /// the waits it withdrew.
    Parked {
        state: ParkedPage,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        issued: Vec<Issued>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        withdrawn: Vec<u64>,
        /// The segment's statement starts and empty-block entries, by
        /// document site ordinal, at `observe: counts` (FIG-6370).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ran: lash_kernel_vm::RanDelta,
        /// The segment's host reads, at `observe: counts`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reads: Option<HostReads>,
        /// Where the segment started, at `observe: counts`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Box<RecordedStart>>,
    },
    /// The run ended.
    Ended {
        #[serde(serialize_with = "lash_protocol_types::durable_serde::serialize")]
        outcome: Box<lash_execution_contracts::process::ProcessOutcome>,
        /// The segment's statement starts and empty-block entries, by
        /// document site ordinal, at `observe: counts` (FIG-6370).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ran: lash_kernel_vm::RanDelta,
        /// The segment's host reads, at `observe: counts`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reads: Option<HostReads>,
        /// Where the segment started, at `observe: counts`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Box<RecordedStart>>,
    },
}

impl KernelRunOutput {
    /// What the segment recorded for observation.
    pub(crate) fn observed(&self) -> Observed<'_> {
        match self {
            Self::Parked {
                ran, reads, start, ..
            }
            | Self::Ended {
                ran, reads, start, ..
            } => Observed {
                ran,
                reads: reads.as_ref(),
                start: start.as_deref(),
            },
        }
    }
}

/// A refused effect's error, as the machine is handed it.
pub(crate) fn refused(kind: &str, message: String) -> OutcomeWire {
    OutcomeWire::Failed(lash_kernel_doc::ErrorDatum {
        kind: kind.to_owned(),
        message,
        data: Datum::Null,
    })
}
