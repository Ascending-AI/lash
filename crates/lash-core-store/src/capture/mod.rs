//! Capture materialization shared by every store and the runtime (ADR 0114
//! §3.1).

mod reducer;

pub use reducer::{CaptureReduceViolation, reduce_capture};
