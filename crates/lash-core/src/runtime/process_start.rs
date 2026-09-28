//! The process-start relay lives in `lash-core-execution`, where the port it
//! delivers through ([`ProcessWorkSubstrate`](crate::ProcessWorkSubstrate)) is
//! defined. This module keeps it at its long-standing path.

pub use lash_core_execution::runtime::process_start::*;
