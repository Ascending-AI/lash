//! Bound source-verified tables used by production region execution.
mod acquire;
mod bind;
mod running;
pub(super) use bind::Code;
pub(super) use running::Running;
