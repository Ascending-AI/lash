//! A parked run as checkpoint graph state (FIG-5887 §2, §5).
//!
//! The worker exports a run as a graph: a KernelWhole root (a TerminalWhole
//! once the run ended) and the canonical nodes it staged, each named by its
//! digest. [`SealedGraphState`] carries them to the parent under a seal that
//! binds the root and the staged node set to their structural facts: owner,
//! kernel version and document. The parent checks those facts and the seal
//! here, and the generic envelope framing of each node with the store's
//! graph module; it never decodes a guest frame. Guest semantics are
//! validated in the worker, which alone decodes kernel state.
//!
//! This crate has no hash of the graph's own: node digests are computed and
//! checked by `lash_core_store::checkpoint_graph`, which depends on this
//! crate.

use lash_sansio::VersionRange;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::state::{StateDigest, VmOwner};

/// One staged node: its digest and its complete envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNodeBytes {
    #[serde(with = "serde_bytes")]
    pub id: [u8; 32],
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
}

/// The root a graph state stands on: its node kind and digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphRoot {
    pub kind: u32,
    #[serde(with = "serde_bytes")]
    pub id: [u8; 32],
}

/// A run's graph state as the parent holds it: a root, the nodes staged for
/// it, and a seal over both and the state's structural facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedGraphState {
    owner: VmOwner,
    /// The kernel version the run was parked under.
    kernel: u32,
    /// The identity of the document the run was parked under.
    document: String,
    root: GraphRoot,
    /// Strictly ascending by digest. Nodes an acknowledged root already
    /// installed may be omitted; the commit rechecks every reference.
    nodes: Vec<GraphNodeBytes>,
    seal: StateDigest,
}

/// What a holder of graph state expects it to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStateExpectation<'a> {
    pub owner: &'a VmOwner,
    /// The kernel versions the holder's workers resume.
    pub kernel: VersionRange,
    /// The document the run is resumed under, when the holder knows it.
    pub document: Option<&'a str>,
    /// The node kinds the root may be.
    pub root_kinds: &'a [u32],
    pub max_nodes: u64,
    /// The staged nodes' envelope bytes in total.
    pub max_bytes: u64,
}

/// Why graph state failed its structural check.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GraphStateRefusal {
    #[error("the graph state belongs to `{found}`, expected `{expected}`")]
    WrongOwner { expected: VmOwner, found: VmOwner },
    #[error("the graph state was written under kernel version {found}, outside read range {reads}")]
    KernelOutsideReadRange { found: u32, reads: VersionRange },
    #[error("the graph state was parked under document {found}, expected {expected}")]
    WrongDocument { expected: String, found: String },
    #[error("the graph state's root is node kind {found}, which this holder does not resume")]
    WrongRootKind { found: u32 },
    #[error("the graph state stages {len} nodes, over the {limit}-node bound")]
    TooManyNodes { limit: u64, len: u64 },
    #[error("the graph state stages {len} bytes, over the {limit}-byte bound")]
    TooLarge { limit: u64, len: u64 },
    #[error("the graph state's nodes are not in strictly ascending digest order")]
    UnsortedNodes,
    #[error("the graph state's seal does not match its contents")]
    SealMismatch,
}

/// The kernel's precise refusal of a state it will not resume, carried typed
/// from the worker to the parent and into the store's graph refusals.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KernelRestoreRefusal {
    #[error("the document is not admitted: {reason}")]
    NotAdmitted { reason: String },
    #[error("the state was parked under document {parked}, not {given}")]
    DocumentMismatch { parked: String, given: String },
    #[error(
        "the state was parked under kernel version {parked}; the document is written for {supported}"
    )]
    KernelVersion { parked: u32, supported: u32 },
    #[error("the registry has no function {function}, which the parked run pins")]
    MissingFunction { function: String },
    #[error("a parked wait names effect {effect}, which no admission under this execution holds")]
    AdmissionUnresolved { effect: String },
    #[error("the parked state is malformed: {problem}")]
    Malformed { problem: String },
    /// The run pins a release manifest this worker does not hold exactly.
    #[error("the run pins release manifest {pinned}, which this worker does not hold")]
    ReleaseMismatch { pinned: String },
    /// The pinned release or this worker lacks a function the run calls.
    #[error("function {function} is not held by the pinned release or this worker")]
    MissingHelper { function: String },
    /// The pinned release records a native implementation this worker
    /// lacks.
    #[error("function {function} needs a native implementation this worker lacks")]
    MissingNative { function: String },
    /// The admission under this execution holds a request of another kind
    /// than the wait's.
    #[error("the admission of effect {effect} is not the kind of the parked wait")]
    AdmissionMismatch { effect: String },
}

fn seal_of(
    owner: &VmOwner,
    kernel: u32,
    document: &str,
    root: &GraphRoot,
    nodes: &[GraphNodeBytes],
) -> StateDigest {
    let mut facts =
        Vec::with_capacity(96 + owner.as_str().len() + document.len() + 32 * nodes.len());
    for part in [owner.as_str().as_bytes(), document.as_bytes()] {
        facts.extend_from_slice(&(part.len() as u64).to_be_bytes());
        facts.extend_from_slice(part);
    }
    facts.extend_from_slice(&kernel.to_be_bytes());
    facts.extend_from_slice(&root.kind.to_be_bytes());
    facts.extend_from_slice(&root.id);
    facts.extend_from_slice(&(nodes.len() as u64).to_be_bytes());
    for node in nodes {
        facts.extend_from_slice(&node.id);
    }
    StateDigest::of_graph_facts(&facts)
}

impl SealedGraphState {
    /// Seals what the worker exported. Nodes are put in digest order; one
    /// digest staged twice is kept once.
    pub fn seal(
        owner: VmOwner,
        kernel: u32,
        document: impl Into<String>,
        root: GraphRoot,
        mut nodes: Vec<GraphNodeBytes>,
    ) -> Self {
        nodes.sort_by_key(|node| node.id);
        nodes.dedup_by(|left, right| left.id == right.id);
        let document = document.into();
        let seal = seal_of(&owner, kernel, &document, &root, &nodes);
        Self {
            owner,
            kernel,
            document,
            root,
            nodes,
            seal,
        }
    }

    /// The structural check the parent makes before it hands the nodes to
    /// the store's envelope check.
    ///
    /// # Errors
    ///
    /// The first mismatch, in the order of [`GraphStateRefusal`]'s variants.
    pub fn check(&self, expected: &GraphStateExpectation<'_>) -> Result<(), GraphStateRefusal> {
        if &self.owner != expected.owner {
            return Err(GraphStateRefusal::WrongOwner {
                expected: expected.owner.clone(),
                found: self.owner.clone(),
            });
        }
        if !expected.kernel.contains(self.kernel) {
            return Err(GraphStateRefusal::KernelOutsideReadRange {
                found: self.kernel,
                reads: expected.kernel,
            });
        }
        if let Some(document) = expected.document
            && document != self.document
        {
            return Err(GraphStateRefusal::WrongDocument {
                expected: document.to_owned(),
                found: self.document.clone(),
            });
        }
        if !expected.root_kinds.contains(&self.root.kind) {
            return Err(GraphStateRefusal::WrongRootKind {
                found: self.root.kind,
            });
        }
        let len = self.nodes.len() as u64;
        if len > expected.max_nodes {
            return Err(GraphStateRefusal::TooManyNodes {
                limit: expected.max_nodes,
                len,
            });
        }
        let bytes = self.byte_len();
        if bytes > expected.max_bytes {
            return Err(GraphStateRefusal::TooLarge {
                limit: expected.max_bytes,
                len: bytes,
            });
        }
        if self.nodes.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err(GraphStateRefusal::UnsortedNodes);
        }
        if seal_of(
            &self.owner,
            self.kernel,
            &self.document,
            &self.root,
            &self.nodes,
        ) != self.seal
        {
            return Err(GraphStateRefusal::SealMismatch);
        }
        Ok(())
    }

    pub fn owner(&self) -> &VmOwner {
        &self.owner
    }

    pub fn kernel(&self) -> u32 {
        self.kernel
    }

    pub fn document(&self) -> &str {
        &self.document
    }

    pub fn root(&self) -> GraphRoot {
        self.root
    }

    pub fn nodes(&self) -> &[GraphNodeBytes] {
        &self.nodes
    }

    pub fn into_nodes(self) -> Vec<GraphNodeBytes> {
        self.nodes
    }

    /// The staged nodes' envelope bytes in total.
    pub fn byte_len(&self) -> u64 {
        self.nodes.iter().map(|node| node.bytes.len() as u64).sum()
    }
}

#[cfg(test)]
#[path = "graph_state_tests.rs"]
mod tests;
