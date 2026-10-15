//! Connection adapters: snapshot boundaries, clock sampling and store-origin tags.
use super::*;

impl SqliteDurableStore {
    pub(super) fn instant(&self) -> Result<DurableInstant, DurableError> {
        integer(self.clock.timestamp_ms())
            .map(DurableInstant)
            .map_err(store_failure)
    }

    pub(super) async fn write<T, F>(&self, label: CommitLabel, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>, DurableInstant) -> Flow<T> + Send + 'static,
    {
        let now = self.instant()?;
        tracing::trace!(label = label.as_str(), "durable sqlite commit");
        self.observer
            .observe_sql(label.as_str(), "sqlite", async {
                self.conn
                    .write_flow(move |tx| {
                        crate::observed_sql::with_origin(std::any::type_name::<F>(), || {
                            body(tx, now)
                        })
                    })
                    .await
                    .map_err(store_failure)?
            })
            .await
    }

    pub(super) async fn read<T, F>(&self, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<Result<T, DurableError>> + Send + 'static,
    {
        self.conn
            .read(move |tx| {
                crate::observed_sql::with_origin(std::any::type_name::<F>(), || body(tx))
            })
            .await
            .map_err(store_failure)?
    }

    /// A single statement owns its SQLite snapshot. Compound reads keep
    /// using `read` so their rows and graph bodies share one transaction.
    pub(super) async fn query<T, F>(&self, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<Result<T, DurableError>> + Send + 'static,
    {
        self.conn
            .call(move |conn| {
                crate::observed_sql::with_origin(std::any::type_name::<F>(), || body(conn))
            })
            .await
            .map_err(store_failure)?
    }
}
