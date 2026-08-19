//! Runs the backend-agnostic durable effect-group suite against the SQLite
//! tier (FIG-1564).
//!
//! The suite itself lives in `lash-core` and is the same one the in-memory
//! reference host answers, so the two tiers are held to one contract rather
//! than two copies of it. It sits in its own integration test rather than
//! alongside the process-registry conformance run because the group laws open
//! and close many hosts over one database file, which is a different fixture
//! shape — and because the shared file has a line budget the laws would push
//! past.

use std::future::Future;
use std::sync::Arc;

use lash_core::{EffectHost, GroupExecutors};
use lash_sqlite_store::SqliteEffectHost;

/// Blocks on `future` from a synchronous context.
///
/// The suite's host factory is synchronous by design — a host is a value, not
/// an await — so opening a store-backed one needs a runtime of its own.
fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

/// One host over `path`, registered with the suite's executor resolver.
///
/// Registration is what makes the host support groups at all: since FIG-1578 a
/// group carries envelopes, and what runs a child is the resolver its host was
/// built with.
fn host(path: &std::path::Path, executors: Arc<dyn GroupExecutors>) -> SqliteEffectHost {
    let path = path.to_path_buf();
    let host = sync_await(async move {
        SqliteEffectHost::open(&path)
            .await
            .expect("SQLite effect-group host")
    });
    host.register_group_executors(executors)
        .expect("a freshly opened host has no resolver yet");
    host
}

/// The durable SQLite tier answers the effect-group contract the same way the
/// in-memory reference host does (FIG-1564).
///
/// One host object per law, all over one database file: the group surface is a
/// property of the store, not of a particular controller instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_effect_host_satisfies_the_effect_group_contract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("effect-groups.db");
    lash_core::testing::conformance::effect_group_host_conformance(|executors| {
        Arc::new(host(&path, executors)) as Arc<dyn EffectHost>
    })
    .await;
}

/// A cancelled child's cancellation is journaled as its terminal, and a host
/// that was not running when the close happened reads it back (FIG-1564).
///
/// The reading host is the point: it holds none of the closing host's memory,
/// so the terminal it serves came out of the effect journal or from nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_journals_a_cancelled_child_as_its_terminal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cancelled-child-terminal.db");
    lash_core::testing::conformance::effect_group_cancelled_child_terminal_is_durable(
        |executors| Arc::new(host(&path, executors)) as Arc<dyn EffectHost>,
    )
    .await;
}
