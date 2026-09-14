//! Effect fixtures shared by more than one relocated runtime test binary.
//!
//! `runtime::tests::effect` owned these while every suite compiled into one
//! unit-test target; the suites that reach for them now live in different
//! binaries, so they sit here and each binary re-exports them under the
//! historical `runtime::tests::effect` path.

pub(crate) use crate::runtime::tests::*;

pub(crate) mod effect_controller_doubles;
pub(crate) mod effect_recording_authority;
