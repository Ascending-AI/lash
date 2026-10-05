//! Bounded durable reads share one continuation shape.

/// A bounded durable read. Apply `changes` before persisting `next`. A
/// retained feed exposes its oldest readable cursor through `retained_after`;
/// a backend that does not expose its horizon returns `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangePage<Change, Cursor> {
    pub changes: Vec<Change>,
    pub next: Cursor,
    pub retained_after: Option<Cursor>,
}
