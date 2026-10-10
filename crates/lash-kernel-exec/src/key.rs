//! Artifact keys: what compiled code is derived from.
//!
//! Code is disposable: deleting every artifact changes only speed. An
//! [`ArtifactKey`] binds an artifact's bytes to every input that shaped
//! them: the code's identity, the transitive identities of what it calls
//! and how each runs, the kernel version, the ABI, the code generator, the
//! partial evaluator, the target and the flags. A key is never an
//! authenticity proof; trust comes from where the bytes came from. Document
//! text, run state, heap ids, addresses and cache state are never inputs.

use std::collections::BTreeMap;

use lash_kernel_doc::{DocumentId, FunctionId, Site, Unit};

/// The blake3 hash of a [`KeyInput`]'s canonical form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactKey(pub [u8; 32]);

/// The digest of the facts a residual variant was specialized under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VariantFactDigest(pub [u8; 32]);

/// The code an artifact is compiled from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Subject {
    /// A library function's generic body.
    Library(FunctionId),
    /// A residual variant of a library function's body.
    Residual(FunctionId, VariantFactDigest),
    /// A document's code by the site of its body: `main`, a declared
    /// function or a closure.
    Document(DocumentId, Site),
}

/// How a callee runs in the registry the code was compiled against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LibRunKind {
    /// Its native implementation, by the native's probe fingerprint.
    Native { probe: [u8; 32] },
    /// Its kernel-code body.
    Body,
    /// An operation of the machine itself.
    Machine,
}

/// The target code is compiled for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TargetId {
    /// The target triple, such as `x86_64-unknown-linux-gnu`.
    pub triple: Box<str>,
    /// The CPU feature baseline, such as `x86-64-v2`.
    pub cpu: Box<str>,
    pub page_size: u32,
    pub endian: Endian,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Endian {
    Little,
    Big,
}

/// Code generation choices that change the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CodeFlags {
    pub opt: OptLevel,
    pub maps: MapDetail,
    /// Whether loop back-edge Boundaries test the watchdog flag.
    pub watchdog: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OptLevel {
    /// Slots stay canonical and every anchor is precise.
    Baseline,
    /// Registers may hold slots inside certified regions.
    Optimized,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MapDetail {
    /// The maps that materialize frames at exits.
    Exits,
    /// Those, plus each op's origin for logical stack traces.
    Origins,
}

/// Everything an artifact's bytes are derived from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub subject: Subject,
    /// Every callee the code calls directly, folds or inlines,
    /// transitively, and how it runs.
    pub deps: BTreeMap<FunctionId, LibRunKind>,
    /// The kernel version: its cost table and rules.
    pub kernel: u32,
    /// [`crate::EXEC_ABI_DIGEST`] of the build that compiled the code.
    pub exec_abi: [u8; 32],
    /// The code generator's version, its compiler crates' versions and its
    /// pass pipeline.
    pub codegen: [u8; 32],
    /// The partial evaluator's version and bounds: residuals only.
    pub pe: Option<[u8; 32]>,
    pub target: TargetId,
    pub flags: CodeFlags,
}

const DOMAIN: &[u8] = b"lash-kernel-code\0";

impl KeyInput {
    /// The key: `blake3("lash-kernel-code\0" ‖ canonical form)`.
    pub fn key(&self) -> ArtifactKey {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DOMAIN);
        hasher.update(&self.canonical());
        ArtifactKey(*hasher.finalize().as_bytes())
    }

    /// The canonical form: every field in declaration order, each variable
    /// part tagged and length-prefixed, so distinct inputs never encode
    /// alike.
    pub fn canonical(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match &self.subject {
            Subject::Library(id) => {
                out.push(0);
                out.extend_from_slice(id.as_bytes());
            }
            Subject::Residual(id, facts) => {
                out.push(1);
                out.extend_from_slice(id.as_bytes());
                out.extend_from_slice(&facts.0);
            }
            Subject::Document(document, site) => {
                out.push(2);
                out.extend_from_slice(document.as_bytes());
                site_bytes(site, &mut out);
            }
        }
        len(self.deps.len(), &mut out);
        for (id, run) in &self.deps {
            out.extend_from_slice(id.as_bytes());
            match run {
                LibRunKind::Native { probe } => {
                    out.push(0);
                    out.extend_from_slice(probe);
                }
                LibRunKind::Body => out.push(1),
                LibRunKind::Machine => out.push(2),
            }
        }
        out.extend_from_slice(&self.kernel.to_le_bytes());
        out.extend_from_slice(&self.exec_abi);
        out.extend_from_slice(&self.codegen);
        match &self.pe {
            None => out.push(0),
            Some(pe) => {
                out.push(1);
                out.extend_from_slice(pe);
            }
        }
        text(&self.target.triple, &mut out);
        text(&self.target.cpu, &mut out);
        out.extend_from_slice(&self.target.page_size.to_le_bytes());
        out.push(match self.target.endian {
            Endian::Little => 0,
            Endian::Big => 1,
        });
        out.push(match self.flags.opt {
            OptLevel::Baseline => 0,
            OptLevel::Optimized => 1,
        });
        out.push(match self.flags.maps {
            MapDetail::Exits => 0,
            MapDetail::Origins => 1,
        });
        out.push(u8::from(self.flags.watchdog));
        out
    }
}

fn len(n: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(&(n as u64).to_le_bytes());
}

fn text(s: &str, out: &mut Vec<u8>) {
    len(s.len(), out);
    out.extend_from_slice(s.as_bytes());
}

fn site_bytes(site: &Site, out: &mut Vec<u8>) {
    match &site.unit {
        Unit::Main => out.push(0),
        Unit::Function(name) => {
            out.push(1);
            text(name.as_str(), out);
        }
        Unit::Library(id) => {
            out.push(2);
            out.extend_from_slice(id.as_bytes());
        }
    }
    len(site.path.len(), out);
    for step in &site.path {
        out.extend_from_slice(&step.to_le_bytes());
    }
}
