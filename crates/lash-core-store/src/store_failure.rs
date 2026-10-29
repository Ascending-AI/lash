//! A failure of a durable store itself, as opposed to a refusal by its
//! rules. It lives here, below `lash-durable`, so the checkpoint graph's
//! refusals carry it typed; `lash-durable` re-exports it at its own path.

/// What kind of store failure a [`StoreFailure`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreFailureKind {
    /// Lock contention or a busy database: retrying may succeed.
    Contended,
    /// The database could not be reached or the transaction did not finish.
    /// Whether it committed is unknown.
    Unavailable,
    /// A stored row did not decode: a defect or foreign data.
    Corrupt,
    /// A newer release finalized the store: this build may not write again.
    WriterRetired,
}

/// A failure of the store itself, not a refusal by the port's rules.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("durable store {kind:?}: {message}")]
pub struct StoreFailure {
    /// What kind of failure.
    pub kind: StoreFailureKind,
    /// Native database code retained through intermediate durable ports.
    pub code: Option<crate::store::StorageCode>,
    /// The driver's account of it.
    pub message: String,
}
