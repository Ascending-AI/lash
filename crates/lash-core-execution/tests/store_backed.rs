//! The crate's tests that need a concrete store or effect host, over a SQLite
//! memory store set (ADR 0102), under `kernel::` with their original module
//! paths and test names.
//!
//! They cannot stay in-crate: lash-sqlite-store depends on this crate, so a
//! `cfg(test)` module that used it would link a second copy of the kernel
//! whose traits are distinct from the ones under test. The crate root below
//! re-exports the kernel's root, its facade support and the `testing`-gated
//! internals seam, so a relocated test reaches `crate::X` exactly as it did
//! in-crate.

#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "relocated unit-test fixtures assert that their setup is valid; in-crate they sat in `cfg(test)` modules the lints exempt"
)]

pub use lash_core_execution::JsonSchema;
/// The relocated tests derive intent and start keys as the kernel does.
pub use lash_core_execution::core_internal::StartKeyDerivation;
pub use lash_core_execution::facade_support::*;
pub use lash_core_execution::testing::kernel_internals::*;
pub use lash_core_execution::*;

#[path = "store_backed/support.rs"]
mod support;

#[path = "store_backed/kernel/mod.rs"]
mod kernel;

/// Scripted deployment operations obey the same fail-before contract as runtime operations.
#[tokio::test]
async fn scripted_turn_changes_returns_the_injected_fault_before_entering_sqlite() {
    use std::num::NonZeroUsize;
    use testing::{DeploymentOp, FaultKind, Outcome, Phase, Script};

    let stores = support::sqlite_memory_store_set().await;
    let script = Script::new();
    let store = script.wrap("reader", stores.session_store_factory());
    script
        .on(DeploymentOp::turns_changed_since)
        .before()
        .fail(|| StoreError::UnsupportedStoreOperation {
            operation: "injected-turn-feed",
        });

    let answer = DeploymentStore::turns_changed_since(
        store.as_ref(),
        store::TurnChangeCursor::initial(),
        NonZeroUsize::MIN,
    )
    .await;
    assert!(
        matches!(
            answer,
            Err(StoreError::UnsupportedStoreOperation {
                operation: "injected-turn-feed"
            })
        ),
        "{answer:?}"
    );
    assert_eq!(script.calls(DeploymentOp::turns_changed_since), 1);
    let trace = script.trace();
    assert_eq!(trace.len(), 1, "fail-before never enters the inner store");
    assert_eq!(trace[0].phase, Phase::Before);
    assert_eq!(trace[0].outcome, Outcome::Failed(FaultKind::Permanent));
}
