//! The obligation relay lives in `lash-core-execution`, where the ports it
//! executes ([`ProcessWorkSubstrate`](crate::ProcessWorkSubstrate)) are defined.
//! This module keeps it at its long-standing path.

pub use lash_core_execution::runtime::shift::relay::*;
