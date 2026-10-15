//! The lash kernel's document.
//!
//! The kernel is one small, dialect-free language with one meaning
//! (`docs/kernel/semantics.md`). This crate is its vocabulary and nothing
//! that runs: the values, the type grammar, the forms, the document with its
//! manifest and annotation layer, library-function definitions with the
//! interface a native implementation satisfies, content-addressed
//! identities, the JSON encoding, structural validation, the decoding of an
//! inbound datum by a type, and the kernel text notation. A host that only
//! reads documents needs no other crate.
//!
//! It depends on no other lash crate.

mod ast;
mod canonical;
mod correspondence;
mod source_map;
mod source_map_codec;
#[cfg(test)]
mod source_map_tests;
pub use correspondence::{Correspondence, Survivor};
pub use source_map::{SourceLanguage, SourceMap, SourceMapRefusal, SourceSite, Span};
mod codec_variants;
mod data_limits;
mod decode;
pub use decode::{DecodeMismatch, Decoder};
mod document;
pub use data_limits::KernelDataLimits;
mod function;
mod graph_codec;
/// The canonical binary token grammar of checkpoint graph node bodies: a
/// cross-crate seam for the kernel's and the store's binary codecs, not part
/// of the document vocabulary.
#[doc(hidden)]
pub mod graph_tokens;
mod identity;
mod name;
mod native;
mod number;
mod text;
mod types;
mod validate;
mod value;
mod version;

#[cfg(test)]
mod tests;

pub use ast::{
    Action, Atom, Block, Callee, Catch, Closure, Expr, Function, JoinMode, Literal, MapEntry,
    Member, Node, Place, ProjectionRead, RecordEntry, Rhs, Site, Stmt, TryStmt, Unit,
};
pub use canonical::EncodeError;
pub use document::{
    Annotations, DecodeError, Document, KERNEL_VERSION, Label, MAX_NESTING_DEPTH, Manifest,
    NodeAnnotation,
};
pub use function::{
    FIRST_NATIVE_VERSION, Formula, FunctionBody, FunctionDeclarations, FunctionDefinition, Guard,
    Implementation, Measure, Operand,
};
pub use graph_codec::{
    FROZEN_DATUM, FROZEN_JSON, FROZEN_TOOL_VALUE, FrozenDatum, MAX_DECODE_DEPTH,
};
pub use identity::{EffectIdentity, LoopIteration, SpawnIdentity, TaskIdentity};
pub use name::{
    DocumentId, EffectName, FunctionId, FunctionName, InvalidIdentity, InvalidQualifiedName, Name,
    QualifiedName,
};
pub use native::{
    Element, FunctionCatalog, FunctionRegistry, GuardExceeded, NativeCall, NativeError,
    NativeFunction, NativeHeap, PreparedNative, RegisteredFunction, RegistryError,
    ValidatedFunctions, WorkCounter,
};
pub use number::{
    Float, Integer, InvalidFloat, InvalidInteger, InvalidNumberToken, NumberPolicy, NumberToken,
};
pub use text::{
    ParseError, ParseErrorReason, parse_definition, parse_definition_with_limits, parse_document,
    parse_document_with_limits, print_definition, print_document,
};
pub use types::{MapType, Param, RecordType, RecordTypeField, Signature, Type};
pub use validate::{
    Invalid, InvalidReason, StatementForm, StatementRuleViolation, check_statement,
    validate_annotations, validate_definition, validate_definition_with_limits, validate_document,
    validate_document_with_limits,
};
pub use value::{
    Bytes, ClosureObject, Datum, ErrorDatum, ErrorValue, Handle, Identity, InvalidBytes, Object,
    ObjectId, TaskId, Timestamp, Value, ValueKind,
};
pub use version::KernelVersion;

/// Internal registration facts shared by the checker and library assembly.
#[doc(hidden)]
pub mod core_support {
    pub use crate::native::RegisteredFunctionCoreSupport;
}
