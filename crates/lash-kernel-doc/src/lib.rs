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
/// The arena: what a run's objects are while it executes (seams C, D).
/// Its value and native ABI are the crate root's.
pub mod arena;
pub mod document;
mod document_bytes;
pub mod function;
pub mod function_value;
mod graph_codec;
/// The canonical binary token grammar of checkpoint graph node bodies: a
/// cross-crate seam for the kernel's and the store's binary codecs, not part
/// of the document vocabulary.
#[doc(hidden)]
/// The persistent semantic heap's page vocabulary: what pages, saved rows
/// and projection hold (FIG-6195, FIG-6708).
pub mod heap;
mod identified;
pub use document_bytes::{DOCUMENT_GRAMMAR_VERSION, DocumentBytes, DocumentBytesRefusal};
pub use identified::{IdentifiedDocument, document_work};
mod native;
mod number;
/// Generated execution plans for library bodies (FIG-6678): what a build's
/// helper compiler emits and an embedder installs beside the definitions.
pub mod plan;
mod released;
mod requirement;
pub mod scalar;
/// One root-based snapshot of live code: what a saved function and a
/// crossing function share (`K-CLO-004`).
pub mod snapshot;
mod text;
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
    ActExpr, ActKind, Action, AssignOrder, Atom, Block, Callee, Catch, Closure, Expr, Function,
    JoinMode, Literal, MapEntry, Member, Node, NodeMut, Place, ProjectionRead, Protocol,
    RecordEntry, Rhs, Stmt, TryStmt, each_closure_mut,
};
pub use continuation::{LiveRolesError, live_roles, live_roles_in};
pub use document::{
    DecodeError, Document, EncodeError, Entry, KERNEL_VERSION, MAX_NESTING_DEPTH, Manifest,
    Ordinals,
};
pub use function::{
    FIRST_NATIVE_VERSION, Formula, FunctionBody, FunctionDeclarations, FunctionDefinition,
    FunctionHeader, Guard, Implementation, ImplementationKind, Measure, Operand,
};
pub use function_value::{
    FUNCTION_VALUE_ENTRY, FUNCTION_VALUE_TAG, FunctionRepresentation, FunctionValue,
    FunctionValueLimits, FunctionValueRefusal, StartAdapter, StartConvention, carry_start_adapters,
    escape_function_key,
};
pub use graph_codec::FrozenDatum;
pub use heap::{EntityId, FamilyId, Identity};
pub use native::{
    FunctionCatalog, FunctionRegistry, GuardExceeded, RegisteredFunction, RegistryError,
    ValidatedFunctions, WorkCounter,
};
pub use number::{
    Float, Integer, InvalidFloat, InvalidInteger, InvalidNumberToken, NumberPolicy, NumberToken,
};
pub use released::{BodyCensus, ImageError, ReleasedFunctions};
pub use requirement::{performed_by, performed_through};
pub use text::{
    ParseError, ParseErrorReason, parse_definition, parse_definition_with_limits, parse_document,
    parse_document_with_limits, parse_signature, print_definition, print_document,
    print_document_with_ranges,
};
pub use validate::{
    Invalid, InvalidReason, StatementForm, StatementRuleViolation, check_statement,
    validate_definition, validate_definition_with_limits, validate_document,
    validate_document_with_limits,
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
    pub const fn wait_id(ordinal: u64) -> lash_vocabulary::sites::WaitId {
        lash_vocabulary::sites::WaitId(ordinal)
    }
    pub const fn wait_number(wait: lash_vocabulary::sites::WaitId) -> u64 {
        wait.0
    }

    use lash_vocabulary::sites::EffectIdentity;
    use lash_vocabulary::sites::TaskIdentity;
    pub fn effect_id(raw: EffectIdentity) -> lash_vocabulary::sites::EffectId {
        lash_vocabulary::sites::EffectId(raw)
    }
    pub fn effect_parts(id: &lash_vocabulary::sites::EffectId) -> &EffectIdentity {
        &id.0
    }
    pub fn task_label(raw: TaskIdentity) -> lash_vocabulary::sites::TaskLabel {
        lash_vocabulary::sites::TaskLabel(raw)
    }
    pub fn task_parts(id: &lash_vocabulary::sites::TaskLabel) -> &TaskIdentity {
        &id.0
    }

    pub fn execution_path(
        rows: Vec<(lash_vocabulary::sites::TaskIdentity, Vec<u32>)>,
    ) -> lash_vocabulary::observation::ExecutionPath {
        lash_vocabulary::observation::ExecutionPath(
            rows.into_iter()
                .map(|(task, starts)| (lash_vocabulary::sites::TaskLabel(task), starts))
                .collect(),
        )
    }
}

mod frame_array;
pub use frame_array::FrameArray;

pub use native::NativeCtor;

pub mod linked_site;
