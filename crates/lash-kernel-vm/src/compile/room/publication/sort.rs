//! Collection and JSON callbacks use the region's ordinary call instruction.
//! Source operands, captures and results stay in the same lease as the caller.
use super::*;
use crate::compile::{CodeId, Executable};

// Retiring statement schedules still consume the source-route compiler. It
// supplies no callback execution, value evaluation or new region authority.
pub(in crate::compile::room) mod caller;
