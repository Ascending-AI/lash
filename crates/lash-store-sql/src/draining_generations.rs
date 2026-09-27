//! `draining_generations`: the build generations an operator marked draining
//! (FIG-3799). The recovery leader's hand-over duty reads the marks and wakes
//! every live process whose current segment a marked generation admitted.
//!
//! The table lives beside the process registry — the processes it moves are
//! read from the same database.

/// The table's unprefixed name.
pub const TABLE: &str = "draining_generations";

/// A mark as a reader decodes it, by position.
pub const ROW_COLUMNS: &str = "generation, marked_at_ms";

crate::statements! {
    /// `draining_generations` statements both backends issue verbatim.
    pub struct DrainingGenerationStatements @ "draining_generation" {
        /// Mark generation `?1` draining at `?2`; a generation already marked
        /// keeps its first mark, and the write touches no row.
        mark = "INSERT INTO draining_generations (generation, marked_at_ms)
             VALUES (?1, ?2)
             ON CONFLICT (generation) DO NOTHING";

        /// Remove generation `?1`'s mark.
        clear = "DELETE FROM draining_generations WHERE generation = ?1";

        /// Every mark, in generation order.
        select_all = "SELECT generation, marked_at_ms
             FROM draining_generations
             ORDER BY generation";
    }
}
