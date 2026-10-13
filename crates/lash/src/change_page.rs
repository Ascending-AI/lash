//! Bounded durable reads share one continuation shape.

/// A bounded durable read. Apply all `changes` before persisting `next`.
/// When entries carry their own cursors, a host can checkpoint each entry
/// atomically with its projection in the host's database. Lash provides no
/// cross-database transaction. Empty pages still carry continuation and horizon. A
/// retained feed exposes its oldest readable cursor through `retained_after`;
/// a backend that does not expose its horizon returns `None`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangePage<Change, Cursor> {
    pub changes: Vec<Change>,
    pub next: Cursor,
    pub retained_after: Option<Cursor>,
}
