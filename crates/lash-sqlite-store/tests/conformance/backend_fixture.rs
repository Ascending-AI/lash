//! One SQLite backend per fixture, on the substrate a suite instance runs.
//!
//! The conformance suite is registered twice (ADR 0102): once over file
//! backends (`conformance.rs`) and once over named in-memory ones
//! (`conformance_memory.rs`). Every fixture reaches its
//! databases through a [`TestBackend`], so the only difference between the
//! two registrations is which [`Substrate`] the backend is opened on.

use std::future::Future;
use std::sync::Arc;

use lash_core_execution::ExecutionScope;
use lash_sansio::{SessionId, TurnId};
use lash_sqlite_store::{SqliteBackend, SqliteBackendOptions, SqliteDatabase, Store};

/// Which kind of SQLite backend a suite instance runs on.
#[expect(
    dead_code,
    reason = "each conformance test root runs the suite on one substrate, so the other variant is never constructed in that crate"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Substrate {
    File,
    Memory,
}

/// A backend and, for a file one, the directory it lives in.
///
/// Cloning shares both: the directory is removed, and a memory backend's
/// databases released, when the last clone and every handle taken from it
/// have dropped.
#[derive(Clone)]
pub(crate) struct TestBackend {
    backend: SqliteBackend,
    _dir: Option<Arc<tempfile::TempDir>>,
}

impl std::ops::Deref for TestBackend {
    type Target = SqliteBackend;

    fn deref(&self) -> &SqliteBackend {
        &self.backend
    }
}

pub(crate) fn durable_turn_scope(
    session_id: impl Into<SessionId>,
    turn_id: impl Into<TurnId>,
) -> ExecutionScope {
    let session_id = session_id.into();
    ExecutionScope::turn(&session_id, turn_id)
}

/// Fresh, empty attachment byte stores for the root-set laws, each a
/// filesystem store in its own directory under `root`.
pub(crate) fn attachment_bytes(
    root: &tempfile::TempDir,
) -> lash_conformance::AttachmentBytesFactory {
    let root = root.path().to_path_buf();
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Arc::new(move || {
        let ordinal = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Arc::new(
            lash_core_execution::facade_support::FileAttachmentStore::new(
                root.join(format!("bytes-{ordinal}")),
            ),
        ) as Arc<dyn lash_core_execution::AttachmentStore>
    })
}

pub(crate) fn system_clock() -> Arc<dyn lash_core_execution::Clock> {
    Arc::new(lash_core_execution::facade_support::SystemClock)
}

impl TestBackend {
    pub(crate) async fn open(substrate: Substrate) -> Self {
        Self::open_with(substrate, |options| options, system_clock()).await
    }

    pub(crate) async fn open_with_clock(
        substrate: Substrate,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        Self::open_with(substrate, |options| options, clock).await
    }

    /// A backend whose default options for `substrate` were adjusted by
    /// `configure`.
    pub(crate) async fn open_with(
        substrate: Substrate,
        configure: impl FnOnce(SqliteBackendOptions) -> SqliteBackendOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        match substrate {
            Substrate::File => {
                let dir = tempfile::tempdir().expect("file backend tempdir");
                let backend = SqliteBackend::open_with_options_and_clock(
                    dir.path(),
                    configure(SqliteBackendOptions::default()),
                    clock,
                )
                .await
                .expect("open the file backend");
                Self {
                    backend,
                    _dir: Some(Arc::new(dir)),
                }
            }
            Substrate::Memory => Self {
                backend: SqliteBackend::memory_with_options_and_clock(
                    configure(SqliteBackendOptions::memory()),
                    clock,
                )
                .await
                .expect("open the memory backend"),
                _dir: None,
            },
        }
    }

    /// The backend as the one value a law takes every port from.
    pub(crate) fn dyn_backend(&self) -> Arc<dyn lash_core_execution::Backend> {
        Arc::new(self.backend.clone())
    }

    /// [`Self::open`] from synchronous fixture code.
    pub(crate) fn blocking(substrate: Substrate) -> Self {
        sync_await(Self::open(substrate))
    }

    /// Fresh handles on the same databases, with the same options and clock.
    pub(crate) async fn reopen(&self) -> Self {
        Self {
            backend: self.backend.reopen().await.expect("reopen the backend"),
            _dir: self._dir.clone(),
        }
    }

    /// [`Self::reopen`] that reports a refused open instead of panicking.
    pub(crate) async fn try_reopen(&self) -> tokio_rusqlite::Result<SqliteBackend> {
        self.backend.reopen().await
    }

    /// Fresh handles on the same databases, with options adjusted by
    /// `configure` and on `clock`.
    pub(crate) async fn reopen_with(
        &self,
        configure: impl FnOnce(SqliteBackendOptions) -> SqliteBackendOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        Self {
            backend: self
                .backend
                .reopen_with_options_and_clock(configure(self.options().clone()), clock)
                .await
                .expect("reopen the backend with other options"),
            _dir: self._dir.clone(),
        }
    }

    /// A new unbound durable-core store on a connection of its own.
    pub(crate) async fn store(&self) -> Arc<Store> {
        Arc::new(
            self.backend
                .open_store()
                .await
                .expect("open a durable-core store"),
        )
    }

    /// [`Self::store`] from synchronous fixture code.
    pub(crate) fn blocking_store(&self) -> Arc<Store> {
        let backend = self.clone();
        sync_await(async move { backend.store().await })
    }

    /// A raw connection to `database`, for fault injection and inspection. It
    /// waits on contention like every lash connection does.
    pub(crate) fn raw(&self, database: SqliteDatabase) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open(self.database_uri(database))
            .expect("open a raw connection to the backend");
        connection
            .busy_timeout(std::time::Duration::from_secs(15))
            .expect("set the raw connection's busy timeout");
        connection
    }
}

/// Drive `future` to completion on a runtime of its own, from synchronous
/// fixture code that may already be inside a runtime.
pub(crate) fn sync_await<T, F>(future: F) -> T
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
