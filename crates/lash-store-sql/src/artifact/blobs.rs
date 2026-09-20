//! `blobs`: the content-addressed byte store.
//!
//! One row per BLAKE3 content address. Four kinds of writer root a blob —
//! a session head's checkpoint, a node anchor's checkpoint, a checkpoint's
//! component edge, and an artifact pointer — and none of them owns the table,
//! which is why every delete here is conditional on all four and why the
//! reclaim statements are the family's most careful text.

/// The table's unprefixed name.
pub const TABLE: &str = "blobs";

/// Every column, in insert order. The whole row: a content address and the
/// bytes it addresses.
pub const INSERT_COLUMNS: &str = "hash, content";

crate::statements! {
    /// `blobs` statements both backends issue verbatim.
    pub struct BlobStatements @ "blob" {
        /// The stored bytes at content address `?1`, still inside this
        /// crate's storage envelope.
        select_content = "SELECT content FROM blobs WHERE hash = ?1";

        /// Every content address, in the global hash order that both backends
        /// take blob locks in.
        select_all_hashes = "SELECT hash FROM blobs ORDER BY hash ASC";

        /// Delete the blob at `?1` unconditionally: issued by the mark/sweep
        /// collector, which has already proven the address unreachable from
        /// every root.
        delete_by_hash = "DELETE FROM blobs WHERE hash = ?1";
    }
}
