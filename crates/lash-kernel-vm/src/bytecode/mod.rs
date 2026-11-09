//! The core's compiler output (cutover contract §1): the instruction set,
//! the compiled [`program::Program`] and its tables.
//!
//! The compiler takes one admitted document and its admission closure and
//! emits one `Program`; the inline pass expands covered library bodies'
//! guard paths into their callers, one validator pass checks its
//! structure, and one fusion pass then fuses runs of its prepaid blocks. Each
//! machine instance compiles its own.

pub(crate) mod charge_tables;
pub(crate) mod compile;
pub(crate) mod conventions;
pub(crate) mod fuse;
pub(crate) mod inline;
pub(crate) mod lower;
pub(crate) mod names;
pub(crate) mod op;
pub(crate) mod program;
pub(crate) mod site_tables;
pub(crate) mod validate;
