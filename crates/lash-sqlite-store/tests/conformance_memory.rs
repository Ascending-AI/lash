//! Runs the shared conformance suites against SQLite memory store sets: three
//! named `memdb` databases pinned by the store set's anchors per fixture
//! (ADR 0102). The same suite runs over file store sets in `conformance.rs`.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

#[path = "conformance/backend_fixture.rs"]
mod backend_fixture;
#[path = "blob_probe.rs"]
mod blob_probe;
#[path = "conformance/session_history.rs"]
mod session_history;
#[path = "conformance/suite.rs"]
mod suite;

const SUBSTRATE: backend_fixture::Substrate = backend_fixture::Substrate::Memory;

#[tokio::test]
async fn ingress_and_replay_preserve_canonical_subscription_order() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("memory store set");
    lash_conformance::first_ingress_and_replay_share_canonical_subscription_order(
        lash_conformance::TriggerStores::of(&stores),
    )
    .await;
}
