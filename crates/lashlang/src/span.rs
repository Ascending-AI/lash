//! Byte offsets into the text a program was authored in.
//!
//! A span is the only thing the IR keeps from an authored surface. ADR 0096
//! retires the Lashlang syntax, so the spans in a `Program` are now supplied by
//! the TypeScript front-end (or stated outright by a test); nothing in this
//! crate produces them any more. They stay here because the compiler, the VM
//! and the durable continuation all blame errors on one.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}
