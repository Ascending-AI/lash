//! The 1.0 node registry (FIG-5887 §3): explicit kind numbers, never Rust
//! enum ordinals, with each kind's body shape, size bound and owning lane.
//!
//! Numbers are the format. A new kind takes an unused number; none is ever
//! renumbered or reused. Kinds 1000 to 1999 are reserved for a future
//! complete object graph (§14) and are not registered in 1.0.

use std::fmt;
use std::ops::RangeInclusive;

/// A node kind's registry number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeKind(pub u32);

impl fmt::Debug for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.spec() {
            Some(spec) => write!(f, "NodeKind({} {})", self.0, spec.name),
            None => write!(f, "NodeKind({})", self.0),
        }
    }
}

/// How a kind's body is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyShape {
    /// The body is the kind's tokens as listed, with no wrapper and no
    /// placement of large values: the scalar leaves and the stream and index
    /// kinds.
    Direct,
    /// The body is `Inline(tokens)` (sum variant 0) when the whole node fits
    /// its bound, and `Stream(ref<WholeStream>)` (sum variant 1) otherwise.
    /// Text and bytes tokens over [`INLINE_TOKEN_MAX`](super::INLINE_TOKEN_MAX)
    /// are placed in shared leaves.
    Enveloped,
}

/// Which hard bound a kind's complete envelope keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeBound {
    /// [`MAX_NODE_BYTES`](super::MAX_NODE_BYTES).
    Node,
    /// [`MAX_ROOT_BYTES`](super::MAX_ROOT_BYTES): a public root descriptor.
    Root,
}

impl SizeBound {
    #[must_use]
    pub const fn bytes(self) -> u64 {
        match self {
            Self::Node => super::MAX_NODE_BYTES,
            Self::Root => super::MAX_ROOT_BYTES,
        }
    }
}

/// The FIG-5887 §13 lane that owns a kind's typed body codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwningLane {
    /// Seams: this crate's generic graph module and `lash-durable`.
    Seams,
    /// Kernel and worker: the kernel crates' binary codecs.
    Kernel,
    /// Host and rounds: broker, executor envelope and host, rounds.
    Host,
    /// Cell and session adoption.
    Cell,
    /// Graph stores: namespace and artifact adapters.
    Stores,
    /// The integrator: turn lifecycle.
    Integrator,
}

/// One registry row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindSpec {
    pub kind: NodeKind,
    pub name: &'static str,
    pub shape: BodyShape,
    pub bound: SizeBound,
    pub owner: OwningLane,
}

macro_rules! registry {
    ($($konst:ident = $number:literal, $name:literal, $shape:ident, $bound:ident, $owner:ident;)*) => {
        impl NodeKind {
            $(pub const $konst: Self = Self($number);)*
        }

        /// Every 1.0 kind, in number order.
        pub const REGISTRY: &[KindSpec] = &[
            $(KindSpec {
                kind: NodeKind($number),
                name: $name,
                shape: BodyShape::$shape,
                bound: SizeBound::$bound,
                owner: OwningLane::$owner,
            },)*
        ];
    };
}

registry! {
    TEXT_LEAF = 1, "TextLeaf", Direct, Node, Seams;
    BYTES_LEAF = 2, "BytesLeaf", Direct, Node, Seams;
    TEXT_INDEX = 3, "TextIndex", Direct, Node, Seams;
    BYTES_INDEX = 4, "BytesIndex", Direct, Node, Seams;
    WHOLE_STREAM = 5, "WholeStream", Direct, Root, Seams;
    STRUCTURE_BLOCK = 6, "StructureBlock", Direct, Node, Seams;
    STREAM_INDEX = 7, "StreamIndex", Direct, Node, Seams;
    FROZEN_VALUE = 8, "FrozenValue", Enveloped, Node, Kernel;
    OPAQUE_LEAF = 9, "OpaqueLeaf", Enveloped, Node, Seams;
    DOCUMENT = 10, "Document", Enveloped, Node, Kernel;
    ANNOTATIONS = 11, "Annotations", Enveloped, Node, Kernel;
    EFFECT_TABLE = 12, "EffectTable", Enveloped, Node, Host;
    RELEASE_MANIFEST = 13, "ReleaseManifest", Enveloped, Node, Kernel;
    HELPER_DEFINITION = 14, "HelperDefinition", Enveloped, Node, Kernel;
    CELL_ENVELOPE = 15, "CellEnvelope", Enveloped, Node, Host;
    KERNEL_WHOLE = 16, "KernelWhole", Enveloped, Root, Kernel;
    EFFECT_MEMBERSHIP = 17, "EffectMembership", Enveloped, Node, Host;
    ADMISSION = 18, "Admission", Enveloped, Node, Host;
    FROZEN_REQUEST = 19, "FrozenRequest", Enveloped, Node, Host;
    CALL_OUTCOME = 20, "CallOutcome", Enveloped, Node, Host;
    ROUND_FACT = 21, "RoundFact", Enveloped, Node, Host;
    SUMMARY_PAGE = 22, "SummaryPage", Enveloped, Node, Host;
    SUMMARY_INDEX = 23, "SummaryIndex", Direct, Node, Host;
    HOST_ROOT = 24, "HostRoot", Enveloped, Root, Host;
    EVENT_PAGE = 25, "EventPage", Enveloped, Node, Host;
    EVENT_INDEX = 26, "EventIndex", Direct, Node, Host;
    TERMINAL_WHOLE = 27, "TerminalWhole", Enveloped, Node, Kernel;
    CELL_END = 28, "CellEnd", Enveloped, Root, Host;
    SESSION_EXECUTION = 29, "SessionExecution", Enveloped, Root, Cell;
    FUNCTION_BUNDLE = 30, "FunctionBundle", Enveloped, Node, Cell;
    PRESENTATION = 31, "Presentation", Enveloped, Node, Host;
    REPORT_VIEW = 32, "ReportView", Enveloped, Node, Host;
    PHASE_ROOT = 33, "PhaseRoot", Enveloped, Root, Integrator;
    SESSION_MANIFEST = 34, "SessionManifest", Enveloped, Root, Cell;
    NAMESPACE_VALUE = 35, "NamespaceValue", Enveloped, Node, Stores;
    EXEC_ROOT = 36, "ExecRoot", Enveloped, Root, Seams;
    OWNER_BINDING = 37, "OwnerBinding", Enveloped, Node, Seams;
    PROCESS_STATE = 38, "ProcessState", Enveloped, Node, Seams;
    TURN_CONTENT = 39, "TurnContent", Enveloped, Node, Integrator;
    CALL_BODY = 40, "CallBody", Enveloped, Node, Host;
}

impl NodeKind {
    /// Kinds reserved for a future complete object graph (FIG-5887 §14):
    /// never registered in 1.0.
    pub const RESERVED_FUTURE: RangeInclusive<u32> = 1000..=1999;

    /// This kind's registry row, when 1.0 registers it.
    #[must_use]
    pub fn spec(self) -> Option<&'static KindSpec> {
        let index = usize::try_from(self.0).ok()?.checked_sub(1)?;
        REGISTRY.get(index).filter(|spec| spec.kind == self)
    }

    /// Whether this is a placed large value's leaf or index: a child of one
    /// of these four kinds in an enveloped body is a text or bytes value.
    #[must_use]
    pub fn is_placed_value(self) -> bool {
        matches!(
            self,
            Self::TEXT_LEAF | Self::BYTES_LEAF | Self::TEXT_INDEX | Self::BYTES_INDEX
        )
    }
}
