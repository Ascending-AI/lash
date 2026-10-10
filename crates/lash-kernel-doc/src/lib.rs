//! The lash kernel's document.
//!
//! The kernel is one small, dialect-free language with one meaning
//! (`docs/kernel/semantics.md`). This crate is its vocabulary and nothing
//! that runs: the values, the type grammar, the forms, the document with its
//! manifest and annotation layer, library-function definitions with the
//! interface a native implementation satisfies, content-addressed
//! identities, the JSON encoding, structural validation, and the kernel text
//! notation. A host that only reads documents needs no other crate.
//!
//! It depends on no other lash crate.

mod ast;
mod canonical;
mod document;
mod function;
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
    FIRST_NATIVE_VERSION, Formula, FunctionBody, FunctionDefinition, Guard, Implementation,
    Measure, Operand,
};
pub use identity::{EffectIdentity, LoopIteration, SpawnIdentity, TaskIdentity};
pub use name::{
    DocumentId, EffectName, FunctionId, FunctionName, InvalidIdentity, InvalidQualifiedName, Name,
    QualifiedName,
};
pub use native::{
    Element, FunctionCatalog, FunctionRegistry, GuardExceeded, NativeCall, NativeError,
    NativeFunction, NativeHeap, RegisteredFunction, RegistryError, ValidatedFunctions, WorkCounter,
};
pub use number::{
    Float, Integer, InvalidFloat, InvalidInteger, InvalidNumberToken, NumberPolicy, NumberToken,
};
pub use text::{
    ParseError, ParseErrorReason, parse_definition, parse_document, print_definition,
    print_document,
};
pub use types::{MapType, Param, RecordType, RecordTypeField, Signature, Type};
pub use validate::{
    Invalid, InvalidReason, StatementForm, StatementRuleViolation, check_statement,
    validate_annotations, validate_definition, validate_document,
};
pub use value::{
    Bytes, ClosureObject, Datum, ErrorDatum, ErrorValue, Handle, Identity, InvalidBytes, Object,
    ObjectId, TaskId, Timestamp, Value, ValueKind,
};
pub use version::KernelVersion;
