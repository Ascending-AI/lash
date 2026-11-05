//! Bound source-verified tables used by production region execution.
mod acquire;
mod bind;
mod materialize;
mod read;
mod running;
mod scalar;
mod values;
pub(super) use bind::Code;
pub(super) use running::Running;
