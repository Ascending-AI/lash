//! The core's compiler output (cutover contract §1): the instruction set,
//! the compiled [`program::Program`] and its tables.
//!
//! The compiler takes one admitted document and its admission closure and
//! emits one `Program`; one validator pass checks its structure. Each
//! machine instance compiles its own.

pub(crate) mod charge_tables;
pub(crate) mod compile;
pub(crate) mod conventions;
pub(crate) mod lower;
pub(crate) mod names;
pub(crate) mod op;
pub(crate) mod program;
