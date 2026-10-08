//! [`SqliteStore`] open/memory lifecycle plus session head/meta accessors.
//!
//! * Async public reads return `Result` so SQLite and decode failures cannot be
//!   mistaken for missing session state.
//! * A read goes through `self.conn.call(move |c| { ... })`, where the closure
//!   is a *synchronous* rusqlite body returning `rusqlite::Result<T>`.
//! * A read-then-write goes through `self.conn.write(move |tx| { ... })`.
//! * The shared `*_from_conn` helpers in `lib.rs` are synchronous and take a
//!   `&rusqlite::Connection`, so they can be called from inside either closure.
//! * Closures must be `'static` + `Send`: capture owned values (clone strings,
//!   move them in), not borrows of `self`.

use super::*;
use crate::location::DatabaseLocation;
#[cfg(any(test, feature = "testing"))]
use crate::location::validate_file_database_path;
use lash_core_execution::FleetFormatStore;
use lash_sansio::SessionId;

/// The `synchronous` mode of the stores this crate's test-only constructors
/// open: fixtures never outlive their process, so the mode decides nothing.
#[cfg(any(test, feature = "testing"))]
pub(crate) const FIXTURE_SYNCHRONOUS: crate::SqliteSynchronous = crate::SqliteSynchronous::Normal;

impl SqliteStore {
    /// Open a named database file for fixtures that inspect or corrupt raw rows.
    /// Fixture stores run under [`FIXTURE_SYNCHRONOUS`].
    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_for_testing(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_file_with_options_and_clock_for_testing(
            path,
            StoreOptions::standard(FIXTURE_SYNCHRONOUS),
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_with_clock_for_testing(
        path: &Path,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_file_with_options_and_clock_for_testing(
            path,
            StoreOptions::standard(FIXTURE_SYNCHRONOUS),
            clock,
        )
        .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_with_options_for_testing(
        path: &Path,
        options: StoreOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_file_with_options_and_clock_for_testing(
            path,
            options,
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_with_options_and_clock_for_testing(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        validate_file_database_path(path, "SqliteStore test fixture")?;
        Self::open_at(
            &DatabaseLocation::standalone_file(path),
            options,
            clock,
            lash_core_execution::FleetFormat::writable(),
            #[cfg(feature = "testing")]
            crate::testing::ConnectionHooks::default(),
        )
        .await
    }

    /// Open the database file at `path`, creating it with every table of the
    /// deployment if it is absent, under the `synchronous` mode the host
    /// states ([`crate::SqliteSynchronous`]).
    pub async fn open(
        path: &Path,
        synchronous: crate::SqliteSynchronous,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            StoreOptions::standard(synchronous),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            "SqliteStore::open",
        )
        .await
    }

    pub async fn open_with_clock(
        path: &Path,
        synchronous: crate::SqliteSynchronous,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            StoreOptions::standard(synchronous),
            clock,
            "SqliteStore::open_with_clock",
        )
        .await
    }

    pub async fn open_with_options(
        path: &Path,
        options: StoreOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            options,
            Arc::new(lash_core_execution::facade_support::SystemClock),
            "SqliteStore::open_with_options",
        )
        .await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            options,
            clock,
            "SqliteStore::open_with_options_and_clock",
        )
        .await
    }

    async fn open_direct(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
        constructor: &'static str,
    ) -> tokio_rusqlite::Result<Self> {
        let location = crate::location::file_location(path, constructor)?;
        Self::open_at(
            &DatabaseLocation::in_backend(&location, None),
            options,
            clock,
            lash_core_execution::FleetFormat::writable(),
            #[cfg(feature = "testing")]
            crate::testing::ConnectionHooks::default(),
        )
        .await
    }

    /// Open the database file at `path` admitting `writable` as the opening
    /// build's fleet-format writable range.
    ///
    /// Testing seam for FIG-3796's rollout proofs: the recorded row is read
    /// against `writable` rather than this binary's own
    /// [`lash_core_execution::FleetFormat::writable_range`], so a test can
    /// stand in for a build whose range does — or does not — still write the
    /// generation the fleet recorded. Production opens always pass this
    /// build's range.
    #[cfg(feature = "testing")]
    pub async fn open_with_fleet_writable_range_for_testing(
        path: &Path,
        writable: lash_core_execution::compat::VersionRange,
    ) -> Result<Self, lash_core_execution::StoreError> {
        validate_file_database_path(path, "Store").map_err(sqlite_async_error)?;
        let store = Self::open_at(
            &DatabaseLocation::standalone_file(path),
            StoreOptions::standard(FIXTURE_SYNCHRONOUS),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            writable,
            crate::testing::ConnectionHooks::default(),
        )
        .await
        .map_err(sqlite_async_error)?;
        Ok(store)
    }

    /// Open the deployment's database at `core`, provisioning every table of
    /// it when it is new.
    ///
    /// `writable` is the opening build's fleet-format writable range: the
    /// recorded row is admitted against it, so a generation the build cannot
    /// write refuses the open rather than being wound back.
    pub(crate) async fn open_at(
        core: &DatabaseLocation,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
        writable: lash_core_execution::compat::VersionRange,
        #[cfg(feature = "testing")] hooks: crate::testing::ConnectionHooks,
    ) -> tokio_rusqlite::Result<Self> {
        #[cfg(feature = "testing")]
        let inline_calls = hooks.inline_calls;
        #[cfg(feature = "testing")]
        let conn =
            SqliteConnection::open_with_hooks(core.target(), options.connection_policy, hooks)
                .await?;
        #[cfg(not(feature = "testing"))]
        let conn =
            SqliteConnection::open_with_policy(core.target(), options.connection_policy).await?;
        crate::schema::ensure_versioned_schema_with_writable(&conn, writable).await?;
        let mut readers = Vec::with_capacity(options.connection_policy.read_connections.get());
        for _ in 0..options.connection_policy.read_connections.get() {
            let reader = SqliteConnection::open_readonly_configured(
                core.target(),
                options.connection_policy.operational,
            )
            .await?;
            #[cfg(feature = "testing")]
            let reader = reader.with_inline_calls(inline_calls);
            readers.push(reader);
        }
        Ok(Self {
            conn,
            location: core.clone(),
            readers,
            next_reader: AtomicU64::new(0),
            decoded_graph_node_bodies: Arc::new(AtomicU64::new(0)),
            decoded_turn_receipt_bodies: Arc::new(AtomicU64::new(0)),
            clock,
            options,
            commit_count: AtomicU64::new(commit_count_entropy_seed()),
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) async fn open_readonly(core: &DatabaseLocation) -> tokio_rusqlite::Result<Self> {
        // Read-only projections cannot reconcile intents or run a reclamation sweep.
        let conn = SqliteConnection::open_readonly(core.target()).await?;
        let fleet_format = conn
            .call(|conn| crate::compat::recorded_or_current(conn))
            .await?;
        conn.observe_fleet_for_testing(fleet_format);
        let readers = vec![conn.clone()];
        Ok(Self {
            conn,
            location: core.clone(),
            readers,
            next_reader: AtomicU64::new(0),
            decoded_graph_node_bodies: Arc::new(AtomicU64::new(0)),
            decoded_turn_receipt_bodies: Arc::new(AtomicU64::new(0)),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
            options: StoreOptions::standard(FIXTURE_SYNCHRONOUS),
            commit_count: AtomicU64::new(commit_count_entropy_seed()),
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn checkpoint_admission_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        let session_id = session_id.clone();
        let fleet = self.fleet_format();
        self.read_connection()
            .call(move |conn| {
                try_load_session_head_meta_from_conn(conn, &session_id, fleet)
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }

    pub async fn settle_observer_intents(
        &self,
        session_id: &SessionId,
        remaining: Vec<lash_core_execution::facade_support::SessionObserverIntent>,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                    let present = tx
                        .query_row(
                            crate::session_sql::session_sql()
                                .meta
                                .select_state_version
                                .sql(),
                            params![session_id.as_str()],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    if present.is_none() {
                        return Err(StoreError::SessionNotFound {
                            session_id: session_id.clone(),
                        });
                    }
                    crate::session_meta::settle_observer_intents_conn(tx, &session_id, &remaining)
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    pub async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        let selected = session_id.clone();
        self.read_connection()
            .call(move |conn| {
                crate::session_meta::load_session_meta(conn, Some(&selected))
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }
}
