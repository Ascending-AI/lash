//! ECMAScript regular expressions for the lash kernel.
//!
//! Six extension functions (`docs/kernel/design.md` §2.4), each with only a
//! native implementation over kernel data:
//!
//! | Function | Takes | Returns |
//! | --- | --- | --- |
//! | `regex.ecma.compile_check` | `pattern`, `flags` | a regex record |
//! | `regex.ecma.exec` | `regex`, `input` | `{match, lastIndex}` |
//! | `regex.ecma.test` | `regex`, `input` | `{matched, lastIndex}` |
//! | `regex.ecma.match_all` | `regex`, `input` | a list of matches |
//! | `regex.ecma.replace` | `regex`, `input`, `replacement` | `{text, lastIndex}` |
//! | `regex.ecma.split` | `regex`, `input`, `limit`? | a list of texts and nulls |
//!
//! **A regex is data.** It is the record `{brand, pattern, flags,
//! lastIndex}` with a brand of [`BRAND`]. No function changes its
//! arguments: one that ECMAScript defines to write `lastIndex` returns the
//! new value instead, and the caller's dialect writes it to the record.
//! Indexes count UTF-16 code units and are kernel integers.
//!
//! **Charge.** Each definition states its charge as a formula over the
//! sizes of the regex, the input and the result. The formula prices the
//! compile on every call, so a call costs the same whether or not the
//! engine still holds the compiled program.
//!
//! **Guard.** Each matching function states a guard: its unit is
//! [`GUARD_UNIT`] and its limit a formula over the input. The steps are
//! counted by the matcher itself, on a program that is a function of the
//! pattern and flags, so the call that passes the limit is the same call,
//! at the same count, on every engine and with any cache.
//!
//! **Cache.** [`Engine`] keeps compiled patterns, up to a number the
//! embedder sets. It is the embedder's memory and changes no result, charge
//! or failure.
//!
//! The crate depends on `lash-kernel-doc` and `lash-regress` and on no
//! other lash crate.

mod definitions;
mod functions;
mod matcher;
mod pattern;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use lash_kernel_doc::{FunctionId, FunctionRegistry, RegistryError};

pub use definitions::{
    BRAND, CHARGE_BASE, CHARGE_PER_PATTERN_UNIT, GUARD_BASE, GUARD_PER_INPUT_UNIT, GUARD_UNIT,
    LONE_SURROGATE_ERROR, Operation, SYNTAX_ERROR,
};
pub use pattern::{Engine, Flags, MAX_GROUP_NESTING, MAX_PATTERN_UNITS, SyntaxError};

/// The identities of the six functions, as [`register`] registered them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Functions {
    pub compile_check: FunctionId,
    pub exec: FunctionId,
    pub test: FunctionId,
    pub match_all: FunctionId,
    pub replace: FunctionId,
    pub split: FunctionId,
}

impl Functions {
    pub fn get(&self, operation: Operation) -> FunctionId {
        match operation {
            Operation::CompileCheck => self.compile_check,
            Operation::Exec => self.exec,
            Operation::Test => self.test,
            Operation::MatchAll => self.match_all,
            Operation::Replace => self.replace,
            Operation::Split => self.split,
        }
    }
}

/// Registers the six functions with their native implementations, which
/// compile through `engine`.
pub fn register(
    registry: &mut FunctionRegistry,
    engine: &Arc<Engine>,
) -> Result<Functions, RegistryError> {
    let mut register = |operation: Operation| {
        registry.register(
            operation.definition(),
            Some(Arc::new(functions::Function {
                engine: Arc::clone(engine),
                operation,
            })),
        )
    };
    Ok(Functions {
        compile_check: register(Operation::CompileCheck)?,
        exec: register(Operation::Exec)?,
        test: register(Operation::Test)?,
        match_all: register(Operation::MatchAll)?,
        replace: register(Operation::Replace)?,
        split: register(Operation::Split)?,
    })
}
