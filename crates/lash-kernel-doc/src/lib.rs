//! The lash kernel's document.
//!
//! The kernel is one small, dialect-free language with one meaning
//! (`docs/kernel/semantics.md`). This crate is its vocabulary and nothing
//! that runs: the values, the type grammar, the forms, the document with its
//! manifest, optional source map and captions, library-function definitions with the
//! interface a native implementation satisfies, content-addressed
//! identities, the JSON encoding, structural validation, the decoding of an
//! inbound datum by a type, and the kernel text notation. A host that only
//! reads documents needs no other crate.
//!
//! Its neutral leaves live in lash-vocabulary; it depends on no runtime crate.

mod ast;
use lash_vocabulary::sites::{Site, Unit};
mod caption;
mod correspondence;
mod declarations;
mod slots;
mod source_map;
mod source_map_codec;
#[cfg(test)]
mod source_map_tests;
pub use caption::{
    CAPTION_DESCRIPTION_BYTES, CAPTION_TITLE_BYTES, Caption, CaptionEntry, CaptionField,
    CaptionKind, CaptionRefusal, CaptionTarget, Captions, CaptionsRefusal, FieldPath, FieldRoot,
    FieldTarget, ParamTarget, Rekeyed,
};
pub use correspondence::{Correspondence, Survivor};
pub use declarations::{
    CheckedDeclarations, DeclarationRefusal, check_declarations, required_declarations,
};
pub use source_map::{SourceLanguage, SourceMap, SourceMapRefusal, SourceSite, Span};
mod codec_variants;
mod continuation;
mod decode;
pub use decode::{DecodeMismatch, Decoder};
pub mod document;
use lash_vocabulary::data_limits::KernelDataLimits;
/// The arena: what a run's objects are while it executes (seams C, D).
/// Its value and native ABI are the crate root's.
pub mod arena;
mod document_bytes;
pub mod function;
pub mod function_value;
mod graph_codec;
/// The canonical binary token grammar of checkpoint graph node bodies: a
/// cross-crate seam for the kernel's and the store's binary codecs, not part
/// of the document vocabulary.
#[doc(hidden)]
use lash_vocabulary::tokens as graph_tokens;
/// The persistent semantic heap's page vocabulary: what pages, saved rows
/// and projection hold (FIG-6195, FIG-6708).
pub mod heap;
mod identified;
pub use document_bytes::{DOCUMENT_GRAMMAR_VERSION, DocumentBytes, DocumentBytesRefusal};
pub use identified::{IdentifiedDocument, document_work};
use lash_vocabulary::observation;
use lash_vocabulary::sites as identity;
/// One root-based snapshot of live code: what a saved function and a
/// crossing function share (`K-CLO-004`).
pub mod snapshot;
use lash_vocabulary::legacy_names as name;
use observation::ExecutionPath;
mod native;
mod number;
/// Generated execution plans for library bodies (FIG-6678): what a build's
/// helper compiler emits and an embedder installs beside the definitions.
pub mod plan;
mod released;
pub mod scalar;
mod text;
use lash_vocabulary::types;
mod validate;
mod value;
mod version;

#[cfg(test)]
mod tests;

pub use arena::Value;
pub use arena::native::{
    Args, BytesView, NativeCall, NativeError, NativeFunction, NewObject, Nursery, PreparedAnswer,
    PreparedNative, Reservation, Retry, Semantics, SequenceView, TableView, TextView,
};
pub use ast::{
    ActExpr, ActKind, Action, AssignOrder, Atom, Block, Callee, Catch, Closure, Expr, JoinMode,
    Literal, MapEntry, Member, Node, NodeMut, Place, ProjectionRead, Protocol, RecordEntry, Rhs,
    Stmt, TryStmt, each_closure_mut,
};
pub use continuation::{LiveRolesError, live_roles, live_roles_in};
pub use document::{
    DecodeError, Document, EncodeError, Export, FunctionExport, KERNEL_VERSION, MAX_NESTING_DEPTH,
    Ordinals, TypeDef, TypeMember,
};
pub use function::{
    FIRST_NATIVE_VERSION, Formula, FunctionBody, FunctionDeclarations, FunctionDefinition,
    FunctionHeader, Guard, Implementation, ImplementationKind, Measure, Metering, Operand,
};
pub use function_value::{
    CallablePresentation, FUNCTION_VALUE_ENTRY, FUNCTION_VALUE_TAG, FunctionRepresentation,
    FunctionTarget, FunctionValue, FunctionValueLimits, FunctionValueRefusal, StartConvention,
    escape_function_key,
};
pub use graph_codec::FrozenDatum;
pub use heap::{EntityId, FamilyId, Identity};
use identity::{EffectId, PositionLabel, TaskLabel, VisitId, WaitId};
pub(crate) use identity::{EffectIdentity, LoopIteration, SpawnIdentity, TaskIdentity};
use name::{DocumentId, FunctionId, FunctionName, Name, QualifiedName, SourceMapId};
pub use native::{
    FunctionCatalog, FunctionRegistry, GuardExceeded, RegisteredFunction, RegistryError,
    ValidatedFunctions, WorkCounter,
};
pub use number::{
    Float, Integer, InvalidFloat, InvalidInteger, InvalidNumberToken, NumberPolicy, NumberToken,
};
pub use released::{BodyCensus, ImageError, ReleasedFunctions};

pub use text::{
    ParseError, ParseErrorReason, parse_definition, parse_definition_with_limits,
    parse_definition_with_locals, parse_definition_with_locals_and_limits, parse_document,
    parse_document_with_limits, parse_signature, print_definition, print_document,
    print_document_with_ranges,
};
use types::{Signature, Type};
pub use validate::{
    Invalid, InvalidReason, StatementForm, StatementRuleViolation, check_statement,
    validate_callable_signature, validate_definition, validate_definition_with_limits,
    validate_document, validate_document_with_limits,
};
pub use value::{Bytes, Datum, ErrorDatum, Handle, InvalidBytes, TaskId, Timestamp, ValueKind};

pub use version::KernelVersion;

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

mod frame_array;
pub use frame_array::FrameArray;

pub use native::NativeCtor;

pub mod linked_site;
