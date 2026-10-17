//! The lash kernel's document.
//!
//! The kernel is one small, dialect-free language with one meaning
//! (`docs/kernel/semantics.md`). This crate is its vocabulary and nothing
//! that runs: the values, the type grammar, the forms, the document with its
//! manifest and optional source map, library-function definitions with the
//! interface a native implementation satisfies, content-addressed
//! identities, the JSON encoding, structural validation, the decoding of an
//! inbound datum by a type, and the kernel text notation. A host that only
//! reads documents needs no other crate.
//!
//! It depends on no other lash crate.

mod ast;
mod correspondence;
mod declarations;
mod source_map;
mod source_map_codec;
#[cfg(test)]
mod source_map_tests;
pub use correspondence::{Correspondence, Survivor};
pub use declarations::{
    CheckedDeclarations, DeclarationRefusal, check_declarations, required_declarations,
};
pub use source_map::{SourceLanguage, SourceMap, SourceMapRefusal, SourceSite, Span};
mod codec_variants;
mod continuation;
mod data_limits;
mod decode;
pub use decode::{DecodeMismatch, Decoder};
mod document;
pub use data_limits::KernelDataLimits;
/// The arena: what a run's objects are while it executes (seams C, D).
/// Its value and native ABI are the crate root's.
pub mod arena;
mod document_bytes;
mod function;
mod function_value;
mod graph_codec;
/// The canonical binary token grammar of checkpoint graph node bodies: a
/// cross-crate seam for the kernel's and the store's binary codecs, not part
/// of the document vocabulary.
#[doc(hidden)]
pub mod graph_tokens;
/// The persistent semantic heap's page vocabulary: what pages, saved rows
/// and projection hold (FIG-6195, FIG-6708).
pub mod heap;
mod identified;
pub use document_bytes::{DOCUMENT_GRAMMAR_VERSION, DocumentBytes, DocumentBytesRefusal};
pub use identified::{IdentifiedDocument, document_work};
mod identity;
mod observation;
/// One root-based snapshot of live code: what a saved function and a
/// crossing function share (`K-CLO-004`).
pub mod snapshot;
pub use observation::{ExecutionPath, RanCounts, ReplayPath, RunObservation, SiteCounts, TaskPath};
mod name;
mod native;
mod number;
/// Generated execution plans for library bodies (FIG-6678): what a build's
/// helper compiler emits and an embedder installs beside the definitions.
pub mod plan;
mod text;
mod types;
mod validate;
mod value;
mod version;
mod workflow;

#[cfg(test)]
mod tests;

pub use arena::Value;
pub use arena::native::{
    Args, BytesView, NativeCall, NativeError, NativeFunction, NewObject, Nursery, PreparedNative,
    Reservation, Retry, Semantics, SequenceView, TableView, TextView,
};
pub use ast::{
    ActExpr, ActKind, Action, AssignOrder, Atom, Block, Callee, Catch, Closure, Expr, Function,
    JoinMode, Literal, MapEntry, Member, Node, Place, ProjectionRead, Protocol, RecordEntry, Rhs,
    Site, Stmt, TryStmt, Unit, each_closure_mut,
};
pub use continuation::{LiveRolesError, live_roles, live_roles_in};
pub use document::{
    DecodeError, Document, EncodeError, Entry, KERNEL_VERSION, Label, MAX_NESTING_DEPTH, Manifest,
};
pub use function::{
    FIRST_NATIVE_VERSION, Formula, FunctionBody, FunctionDeclarations, FunctionDefinition, Guard,
    Implementation, Measure, Operand,
};
pub use function_value::{
    FUNCTION_VALUE_ENTRY, FUNCTION_VALUE_TAG, FunctionRepresentation, FunctionValue,
    FunctionValueLimits, FunctionValueRefusal, StartAdapter, StartConvention, carry_start_adapters,
    escape_function_key,
};
pub use graph_codec::{
    FROZEN_DATUM, FROZEN_JSON, FROZEN_TOOL_VALUE, FrozenDatum, MAX_DECODE_DEPTH,
};
pub use heap::{EntityId, FamilyId, Identity};
pub use identity::{
    EffectId, LoopCounts, ObservationReader, PositionCounts, PositionLabel, TaskLabel, VisitId,
    WaitId,
};
pub(crate) use identity::{EffectIdentity, LoopIteration, SpawnIdentity, TaskIdentity};
pub use name::{
    DocumentId, EffectName, FunctionId, FunctionName, InvalidIdentity, InvalidQualifiedName, Name,
    QualifiedName, SourceMapId,
};
pub use native::{
    FunctionCatalog, FunctionRegistry, GuardExceeded, RegisteredFunction, RegistryError,
    ValidatedFunctions, WorkCounter,
};
pub use number::{
    Float, Integer, InvalidFloat, InvalidInteger, InvalidNumberToken, NumberPolicy, NumberToken,
};
pub use text::{
    ParseError, ParseErrorReason, parse_definition, parse_definition_with_limits, parse_document,
    parse_document_with_limits, print_definition, print_document, print_document_with_ranges,
};
pub use types::{MapType, Param, RecordType, RecordTypeField, Signature, Type};
pub use validate::{
    Invalid, InvalidReason, StatementForm, StatementRuleViolation, check_statement,
    validate_definition, validate_definition_with_limits, validate_document,
    validate_document_with_limits,
};
pub use value::{Bytes, Datum, ErrorDatum, Handle, InvalidBytes, TaskId, Timestamp, ValueKind};

pub use version::KernelVersion;
pub use workflow::{WorkflowDocumentEntry, WorkflowDocumentRef};

/// Internal registration facts shared by the checker and library assembly.
#[doc(hidden)]
pub mod core_support {
    pub use crate::native::RegisteredFunctionCoreSupport;
}

pub use graph_codec::heap_values;

/// Construction seams shared only by kernel crates.
#[doc(hidden)]
pub mod kernel_internals {
    pub const fn wait_id(ordinal: u64) -> crate::WaitId {
        crate::WaitId(ordinal)
    }
    pub const fn wait_number(wait: crate::WaitId) -> u64 {
        wait.0
    }

    pub use crate::identity::{EffectIdentity, LoopIteration, SpawnIdentity, TaskIdentity};
    pub fn effect_id(raw: EffectIdentity) -> crate::EffectId {
        crate::EffectId(raw)
    }
    pub fn effect_parts(id: &crate::EffectId) -> &EffectIdentity {
        &id.0
    }
    pub fn task_label(raw: TaskIdentity) -> crate::TaskLabel {
        crate::TaskLabel(raw)
    }
    pub fn task_parts(id: &crate::TaskLabel) -> &TaskIdentity {
        &id.0
    }

    pub fn execution_path(rows: Vec<(crate::TaskIdentity, Vec<u32>)>) -> crate::ExecutionPath {
        crate::ExecutionPath(
            rows.into_iter()
                .map(|(task, starts)| (crate::TaskLabel(task), starts))
                .collect(),
        )
    }
}

/// Engine-owned fixtures for host laws of identity equality and transport.
pub mod testkit {
    /// Delivery-address fixtures for host laws of matching and withdrawn waits.
    pub const fn wait_id(ordinal: u64) -> crate::WaitId {
        crate::WaitId(ordinal)
    }

    pub fn root_task() -> crate::TaskLabel {
        crate::TaskLabel(crate::TaskIdentity::Main)
    }
    pub fn spawned_task(
        parent: crate::TaskLabel,
        site: impl Into<crate::PositionLabel>,
        occurrence: u64,
    ) -> crate::TaskLabel {
        crate::TaskLabel(crate::TaskIdentity::Spawned(crate::SpawnIdentity {
            parent: std::sync::Arc::new(parent.0),
            site: site.into().0,
            occurrence,
        }))
    }
    pub const fn visit(ordinal: u64) -> crate::VisitId {
        crate::VisitId(ordinal)
    }
    pub fn effect(
        task: crate::TaskLabel,
        site: impl Into<crate::PositionLabel>,
        occurrence: u64,
        loops: Vec<(crate::Site, u64)>,
    ) -> crate::EffectId {
        crate::EffectId(crate::EffectIdentity {
            task: task.0,
            site: site.into().0,
            occurrence,
            loops: loops
                .into_iter()
                .map(|(site, iteration)| crate::LoopIteration { site, iteration })
                .collect(),
        })
    }
    /// Repeated-visit fixtures for the semantic live-frame folding laws.
    pub fn with_visit(id: &crate::EffectId, visit: u64) -> crate::EffectId {
        let mut raw = id.0.clone();
        raw.occurrence = visit;
        crate::EffectId(raw)
    }
    /// Position fixtures for document-binding and mismatch laws.
    pub fn with_position(
        id: &crate::EffectId,
        position: impl Into<crate::PositionLabel>,
    ) -> crate::EffectId {
        let mut raw = id.0.clone();
        raw.site = position.into().0;
        crate::EffectId(raw)
    }
    /// Fan-out fixtures for host observation attribution laws.
    pub fn with_task(id: &crate::EffectId, task: crate::TaskLabel) -> crate::EffectId {
        let mut raw = id.0.clone();
        raw.task = task.0;
        crate::EffectId(raw)
    }
    /// Enclosing-loop fixtures for the host observation transport laws.
    pub fn with_loops(id: &crate::EffectId, loops: Vec<(crate::Site, u64)>) -> crate::EffectId {
        let mut raw = id.0.clone();
        raw.loops = loops
            .into_iter()
            .map(|(site, iteration)| crate::LoopIteration { site, iteration })
            .collect();
        crate::EffectId(raw)
    }
}
