use std::sync::Arc;
use std::sync::Mutex;

/// A fresh storage backend for tests that do not execute durable effects.
pub(crate) async fn sqlite_recording_backend() -> lash_core::Backend {
    sqlite_memory_store_backend().await
}

std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`sqlite_recording_backend`] for a test that reaches
/// only store ports.
pub(crate) async fn sqlite_memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    TEST_STORE_SETS.with(|held| held.borrow_mut().push(std::sync::Arc::clone(&stores)));
    stores
}

/// [`sqlite_memory_store_set`] as a backend whose effect host is the recording
/// double: for a test that needs a `Backend` value but runs no effect.
pub(crate) async fn sqlite_memory_store_backend() -> lash_core::Backend {
    lash_conformance::backend_over(sqlite_memory_store_set().await)
}

thread_local! {
    /// A fixture's selected SQLite module store. Contexts and process workers
    /// share the slot even when the fixture creates the store after the context.
    static ARTIFACT_SLOT: Arc<Mutex<Option<lash_core::Backend>>> =
        Arc::new(Mutex::new(None));
}

pub(crate) fn sqlite_recording_backend_blocking() -> lash_core::Backend {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build a current-thread runtime")
                    .block_on(sqlite_recording_backend())
            })
            .join()
            .expect("open the memory backend on its own thread")
    })
}

pub(crate) fn sqlite_memory_artifact_store_blocking() -> lashlang::LashlangArtifacts {
    lashlang::LashlangArtifacts::of_backend(&sqlite_recording_backend_blocking())
}

// The executor's TypeScript entry points for cell-level tests: each runs one
// cell under the TypeScript dialect a host would select.

#[cfg(test)]
pub(crate) fn deferred_link() -> lash_lashlang_runtime::DeferredLink {
    lash_lashlang_runtime::DeferredLink::new(lash_lashlang_runtime::DeferredResolutionLinkKey {
        address: lash_core::EffectAddress::new(
            lash_core::ExecutionScope::turn("test-session", "turn-1"),
            "replay:effect-1",
        )
        .expect("valid fixture link"),
    })
}
