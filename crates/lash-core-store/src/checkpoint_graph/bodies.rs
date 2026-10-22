//! The typed bodies of the generic kinds the seam owns (FIG-5887 §3):
//! OpaqueLeaf, PhaseRoot, ExecRoot, OwnerBinding and ProcessState.
//!
//! Field order is the registry's. Structs are sequences of exactly their
//! fields; enums are sums with explicit variant numbers. Every reference is
//! a child of a stated kind, checked by [`ExecRoot::check_children`] and
//! [`ProcessState::check_children`].

use lash_vocabulary::tokens::TokenCodec;
use lash_vocabulary::tokens::TokenError;
use lash_vocabulary::tokens::TokenReader;
use lash_vocabulary::tokens::TokenWriter;

use super::decode::GraphDecoder;
use super::node::NodeId;
use super::refusal::GraphRefusal;
use super::registry::NodeKind;
use lash_vocabulary::providers::PluginCallbackIdentity;

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

/// Kind 9: raw bytes an owning codec reads, with no graph children inside
/// them. The bytes are inline (placed in shared leaves when large), or a
/// declared reference to an opaque blob its node keeps as a leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpaqueLeaf {
    /// The identity of the codec that owns the bytes.
    pub codec: String,
    pub payload: OpaquePayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpaquePayload {
    /// Variant 0: the bytes themselves.
    Inline(Vec<u8>),
    /// Variant 1: an opaque blob (encoding 0) holding them.
    RawBlob(NodeId),
}

impl TokenCodec for OpaqueLeaf {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.codec);
        match &self.payload {
            OpaquePayload::Inline(bytes) => {
                out.sum(0, 1);
                out.bytes(bytes);
            }
            OpaquePayload::RawBlob(blob) => {
                out.sum(1, 1);
                blob.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let codec = input.text()?.to_owned();
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        let payload = match variant {
            0 => OpaquePayload::Inline(input.bytes()?.to_vec()),
            1 => OpaquePayload::RawBlob(NodeId::decode(input)?),
            other => return Err(unsupported(other)),
        };
        Ok(Self { codec, payload })
    }
}

/// Kind 37: whose execution a root is. No mutable lease epoch: ownership is
/// fenced by the owning row, never by the graph. The stored execution key
/// is derived from this identity, not stored beside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OwnerBinding {
    /// Variant 0: a code cell of a session's run.
    Cell {
        session: String,
        turn: String,
        cell: String,
    },
    /// Variant 1: a durable process.
    Process { process: String },
}

impl TokenCodec for OwnerBinding {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Cell {
                session,
                turn,
                cell,
            } => {
                out.sum(0, 3);
                out.text(session);
                out.text(turn);
                out.text(cell);
            }
            Self::Process { process } => {
                out.sum(1, 1);
                out.text(process);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        match variant {
            0 => {
                TokenReader::expect_fields(fields, 3)?;
                Ok(Self::Cell {
                    session: input.text()?.to_owned(),
                    turn: input.text()?.to_owned(),
                    cell: input.text()?.to_owned(),
                })
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Process {
                    process: input.text()?.to_owned(),
                })
            }
            other => Err(unsupported(other)),
        }
    }
}

/// A unit-variant discriminator, written as a sum with no fields.
macro_rules! discriminator {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $number:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant = $number,)*
        }

        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                out.sum(*self as u32, 0);
            }

            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                let (variant, fields) = input.sum()?;
                TokenReader::expect_fields(fields, 0)?;
                match variant {
                    $($number => Ok(Self::$variant),)*
                    other => Err(unsupported(other)),
                }
            }
        }
    };
}

discriminator! {
    /// What an execution root executes.
    ExecRootKind { Cell = 0, Process = 1 }
}

discriminator! {
    /// Whether an execution root is parked or ended.
    ExecStatus { Parked = 0, Ended = 1 }
}

/// Kind 33: a turn phase's checkpoint root. Its checkpoint text excludes
/// its structured window pin and its content, which it names by digest, each a
/// TurnContent or ModelRequestFrame node, ascending. Its recorded run is
/// the shared ConfigValue graph of its resolved shape and the TurnContent node
/// of its installed tool surface.
/// The integrator owns the checkpoint data. A phase root owes nothing:
/// the receipt of a model call whose derivation is owed is its own root
/// (`turn_phases.answer_ref`), never a field of the phase's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseRoot {
    pub checkpoint: String,
    pub window_pin: Option<serde_json::Value>,
    pub content: Vec<(String, NodeId)>,
    pub run: Option<NodeId>,
    pub surface: Option<NodeId>,
}

fn encode_node_ref(node: Option<&NodeId>, out: &mut TokenWriter) {
    match node {
        None => out.none(),
        Some(node) => {
            out.some();
            node.encode(out);
        }
    }
}

fn decode_node_ref(input: &mut TokenReader<'_>) -> Result<Option<NodeId>, TokenError> {
    if input.option()? {
        Ok(Some(NodeId::decode(input)?))
    } else {
        Ok(None)
    }
}

impl TokenCodec for PhaseRoot {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        out.text(&self.checkpoint);
        self.window_pin.encode(out);
        out.seq(self.content.len() as u64);
        for (digest, node) in &self.content {
            out.seq(2);
            out.text(digest);
            node.encode(out);
        }
        encode_node_ref(self.run.as_ref(), out);
        encode_node_ref(self.surface.as_ref(), out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        let checkpoint = input.text()?.to_owned();
        let window_pin = Option::decode(input)?;
        let count = input.seq()?;
        let mut content = Vec::new();
        for _ in 0..count {
            input.expect_struct(2)?;
            let digest = input.text()?.to_owned();
            content.push((digest, NodeId::decode(input)?));
        }
        let run = decode_node_ref(input)?;
        let surface = decode_node_ref(input)?;
        Ok(Self {
            checkpoint,
            window_pin,
            content,
            run,
            surface,
        })
    }
}

/// Kind 36: one execution's complete root descriptor, at most
/// [`MAX_ROOT_BYTES`](super::MAX_ROOT_BYTES). Every root is a complete graph:
/// it never depends on an earlier root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRoot {
    pub exec: ExecRootKind,
    /// An OwnerBinding.
    pub owner: NodeId,
    /// The exact executable identity the state was captured on.
    pub executable_identity: String,
    /// The kernel or engine format number.
    pub format: u32,
    pub status: ExecStatus,
    /// A CheckpointRoot while a cell is parked, a CellEnd once it ended, or a
    /// ProcessState for a process.
    pub state: NodeId,
    /// An EffectMembership.
    pub effect_membership: Option<NodeId>,
    /// A HostRoot.
    pub host: Option<NodeId>,
    /// A CellEnvelope.
    pub envelope: Option<NodeId>,
    /// The callbacks the execution owes beyond its admitted calls while it
    /// is parked (S7): the provider and binding codec of every deferred
    /// match a cell committed, which its later calls run on. Empty once it
    /// ended, and for a process.
    pub owed: Vec<PluginCallbackIdentity>,
    /// What running the execution requires of a node (FIG-6985): the
    /// natives and kernel versions its code reaches and the providers and
    /// codecs of its static operation links, written with every root. An
    /// ended root requires nothing.
    pub requirements: lash_vocabulary::claims::ClaimRequirements,
}

impl TokenCodec for ExecRoot {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(11);
        self.exec.encode(out);
        self.owner.encode(out);
        out.text(&self.executable_identity);
        out.unsigned(u64::from(self.format));
        self.status.encode(out);
        self.state.encode(out);
        self.effect_membership.encode(out);
        self.host.encode(out);
        self.envelope.encode(out);
        self.owed.encode(out);
        self.requirements.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(11)?;
        Ok(Self {
            exec: ExecRootKind::decode(input)?,
            owner: NodeId::decode(input)?,
            executable_identity: input.text()?.to_owned(),
            format: input.unsigned_u32()?,
            status: ExecStatus::decode(input)?,
            state: NodeId::decode(input)?,
            effect_membership: Option::decode(input)?,
            host: Option::decode(input)?,
            envelope: Option::decode(input)?,
            owed: Vec::decode(input)?,
            requirements: lash_vocabulary::claims::ClaimRequirements::decode(input)?,
        })
    }
}

impl ExecRoot {
    /// The kinds a state reference of this root may name.
    #[must_use]
    pub fn state_kinds(&self) -> &'static [NodeKind] {
        match (self.exec, self.status) {
            (ExecRootKind::Cell, ExecStatus::Parked) => &[NodeKind::CHECKPOINT_ROOT],
            (ExecRootKind::Cell, ExecStatus::Ended) => &[NodeKind::CELL_END],
            (ExecRootKind::Process, _) => &[NodeKind::PROCESS_STATE],
        }
    }

    /// Checks every reference names a node of its stated kind.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::WrongChildKind`], or a node read's refusal.
    pub fn check_children(
        &self,
        id: NodeId,
        decoder: &mut GraphDecoder<'_>,
    ) -> Result<(), GraphRefusal> {
        decoder.child_kind(id, self.owner, &[NodeKind::OWNER_BINDING])?;
        decoder.child_kind(id, self.state, self.state_kinds())?;
        for (child, kind) in [
            (self.effect_membership, NodeKind::EFFECT_MEMBERSHIP),
            (self.host, NodeKind::HOST_ROOT),
            (self.envelope, NodeKind::CELL_ENVELOPE),
        ] {
            if let Some(child) = child {
                decoder.child_kind(id, child, &[kind])?;
            }
        }
        Ok(())
    }
}

/// Kind 38: a process engine's state in the graph domain: its own bytes, or
/// a kernel state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessState {
    pub engine: String,
    pub executable_identity: String,
    /// The engine's existing state format number.
    pub state_format: u32,
    pub state: ProcessStateBody,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessStateBody {
    /// Variant 0: the engine's raw state bytes (placed in shared leaves when
    /// large).
    EngineBytes(Vec<u8>),
    /// Variant 1: a kernel engine's state: its own bytes and the
    /// CheckpointRoot of the run they stand on, which the process's root
    /// owns from the advance that names it (FIG-6193).
    Kernel { root: NodeId, bytes: Vec<u8> },
}

impl TokenCodec for ProcessState {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        out.text(&self.engine);
        out.text(&self.executable_identity);
        out.unsigned(u64::from(self.state_format));
        match &self.state {
            ProcessStateBody::EngineBytes(bytes) => {
                out.sum(0, 1);
                out.bytes(bytes);
            }
            ProcessStateBody::Kernel { root, bytes } => {
                out.sum(1, 2);
                root.encode(out);
                out.bytes(bytes);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        let engine = input.text()?.to_owned();
        let executable_identity = input.text()?.to_owned();
        let state_format = input.unsigned_u32()?;
        let (variant, fields) = input.sum()?;
        let state = match variant {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                ProcessStateBody::EngineBytes(input.bytes()?.to_vec())
            }
            1 => {
                TokenReader::expect_fields(fields, 2)?;
                ProcessStateBody::Kernel {
                    root: NodeId::decode(input)?,
                    bytes: input.bytes()?.to_vec(),
                }
            }
            other => return Err(unsupported(other)),
        };
        Ok(Self {
            engine,
            executable_identity,
            state_format,
            state,
        })
    }
}

impl ProcessState {
    /// Checks a kernel state reference names a CheckpointRoot.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::WrongChildKind`], or a node read's refusal.
    pub fn check_children(
        &self,
        id: NodeId,
        decoder: &mut GraphDecoder<'_>,
    ) -> Result<(), GraphRefusal> {
        if let ProcessStateBody::Kernel { root, .. } = &self.state {
            decoder.child_kind(id, *root, &[NodeKind::CHECKPOINT_ROOT])?;
        }
        Ok(())
    }
}
