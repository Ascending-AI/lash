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
//! Its neutral leaves live in lash-kernel-value; it depends on no runtime crate.

mod ast;
pub mod constant_pool;
use {lash_kernel_value::sites::Site, lash_kernel_value::sites::Unit};
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
pub use source_map::{
    NonEmptySpans, SourceLanguage, SourceLocation, SourceMap, SourceMapRefusal, SourceSite, Span,
};
mod codec_variants;
mod continuation;
mod decode;
pub use decode::{DecodeRefusal, Decoder, datum_type, diagnostic_datum};
pub use {
    lash_kernel_value::AnnotationOrigin, lash_kernel_value::AnnotationResolutionReason,
    lash_kernel_value::EntryAnnotationRefusal, lash_kernel_value::EntrySignatureRefusal,
    lash_kernel_value::EntryTypeMismatch, lash_kernel_value::SignatureOrigin,
    lash_kernel_value::TypeMismatch,
};
pub mod document;

/// The arena: what a run's objects are while it executes (seams C, D).
/// Its value and native ABI are the crate root's.
pub mod arena;
mod document_bytes;
pub mod function;
mod function_capture;
mod implicit_text;
pub use implicit_text::{ImplicitTextConversion, SourceOperandType};
pub mod function_value;
pub use function_capture::CaptureValue;
mod graph_codec;
/// The canonical binary token grammar of checkpoint graph node bodies: a
/// cross-crate seam for the kernel's and the store's binary codecs, not part
/// of the document vocabulary.
/// The persistent semantic heap's page vocabulary: what pages, saved rows
/// and projection hold (FIG-6195, FIG-6708).
pub mod heap;
pub mod logical_held;
pub use logical_held::LogicalHeldV1;
mod native_resources;
pub use native_resources::{
    NativeResourceContract, NativeResourceEntity, NativeResourceEvent, NativeResourceExit,
    NativeResourceHolding, NativeResourceOrigin, NativeResourceOwner, NativeResourceRelease,
};
mod identified;
pub use document_bytes::{DOCUMENT_GRAMMAR_VERSION, DocumentBytes, DocumentBytesRefusal};
pub use identified::{IdentifiedDocument, document_work};
/// One root-based snapshot of live code: what a saved function and a
/// crossing function share (`K-CLO-004`).
pub mod snapshot;
use lash_kernel_value::names as name;

mod native;
mod number;
/// Generated execution plans for library bodies (FIG-6678): what a build's
/// helper compiler emits and an embedder installs beside the definitions.
pub mod plan;
mod released;
pub mod scalar;
/// One root-based snapshot of live code: what a saved function and a
/// crossing function share (`K-CLO-004`).
mod text;
mod validate;
mod value;
mod version;

#[cfg(test)]
mod tests;

pub use arena::Value;
pub use arena::native::{
    Args, BytesView, NativeCall, NativeError, NativeFunction, NativeSpecialization, NewObject,
    Nursery, PreparedAnswer, PreparedNative, Reservation, ResidentRead, Retry, Semantics,
    SequenceView, SymbolicNative, TableView, TextView,
};
pub use ast::{
    ActExpr, ActKind, Action, AssignOrder, Atom, Block, Callee, Catch, Closure, Expr, JoinMode,
    Literal, MapEntry, Member, Node, NodeMut, Place, Protocol, RecordEntry, Rhs, Stmt, TryStmt,
    each_closure_mut,
};
pub use continuation::{LiveRolesError, live_roles, live_roles_in};
pub use document::{
    DecodeError, Document, DocumentCatalog, EncodeError, Export, FunctionExport, KERNEL_VERSION,
    LibraryDependencies, MAX_NESTING_DEPTH, Ordinals, RestoreSelection, Selection, TypeDef,
    TypeMember, validate_restored_document_with_limits,
};
pub use function::{
    FIRST_NATIVE_VERSION, Formula, FunctionBody, FunctionDeclarations, FunctionDefinition,
    FunctionHeader, Guard, Implementation, ImplementationKind, Measure, Metering, Operand,
    ProjectionRole,
};
pub use function_value::{
    CallablePresentation, FUNCTION_VALUE_ENTRY, FUNCTION_VALUE_TAG, FunctionLocation,
    FunctionRepresentation, FunctionTarget, FunctionValue, FunctionValueLimits,
    FunctionValueRefusal, SignaturePosition, SignatureRequired, StartConvention, WorkflowReference,
    WrittenSignature, escape_function_key, require_entry_signature,
};
pub use graph_codec::FrozenDatum;
pub use heap::{EntityId, FamilyId, Identity};

use name::{DocumentId, FunctionId};
pub use native::{
    FunctionCatalog, FunctionRegistry, GuardExceeded, RegisteredFunction, RegistryError,
    ValidatedFunctions, WorkCounter,
};
pub use number::{
    Float, Integer, InvalidFloat, InvalidInteger, InvalidNumberToken, NumberPolicy, NumberToken,
};
pub use released::{
    BodyCensus, BuildHelpers, DocumentChecks, HelperUnavailable, ImageError,
    LibraryRequirementRefusal, ReleasedFunctions, helper_imports, library_requirements,
};

pub use text::{
    ParseError, ParseErrorReason, parse_definition, parse_definition_with_limits,
    parse_definition_with_locals, parse_definition_with_locals_and_limits, parse_document,
    parse_document_with_limits, parse_signature, print_definition, print_document,
    print_document_with_ranges,
};

pub use validate::{
    Invalid, InvalidReason, StatementForm, StatementRuleViolation, check_statement,
    validate_callable_signature, validate_definition, validate_definition_with_limits,
    validate_document, validate_document_structure, validate_document_structure_with_limits,
    validate_document_with_limits,
};
pub use value::{Bytes, Datum, ErrorDatum, Handle, InvalidBytes, TaskId, Timestamp, ValueKind};

pub use version::KernelVersion;

/// Internal registration facts shared by the checker and library assembly.
#[doc(hidden)]
pub mod core_support {
    pub use crate::native::RegisteredFunctionCoreSupport;
    pub use crate::text::parser::{
        function_references, parse_definition_over, parse_document_over,
    };
}

pub use graph_codec::heap_values;

/// Construction seams shared only by kernel crates.
#[doc(hidden)]
pub mod kernel_internals {
    pub const fn wait_id(ordinal: u64) -> lash_kernel_value::sites::WaitId {
        lash_kernel_value::sites::WaitId(ordinal)
    }
    pub const fn wait_number(wait: lash_kernel_value::sites::WaitId) -> u64 {
        wait.0
    }
}

mod frame_array;
pub use frame_array::FrameArray;

pub use native::NativeCtor;

pub mod linked_site;

mod helper_upgrade;
pub use helper_upgrade::{
    HelperChange, HelperId, HelperPublicationUpgrade, HelperPublicationUpgradeRequest,
    HelperRunUpgrade, HelperRunUpgradeRequest, HelperRunUpgradeState, HelperUpgrade,
    HelperUpgradeRefusal, HelperUpgradeSafety,
};
