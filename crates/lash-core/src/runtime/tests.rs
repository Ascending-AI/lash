//! The runtime suites that used to live here are now integration test binaries
//! under `crates/lash-core/tests/runtime/tests/`. The fixtures they shared with
//! the crate's own unit tests moved to `crate::testing`; this module keeps the
//! historical `crate::runtime::tests::helpers` path pointing at them.

pub(crate) mod helpers {
    pub(crate) use crate::testing::runtime_helpers::*;
}
