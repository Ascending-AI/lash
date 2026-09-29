//! `attachment_sweep_clock`: the single-row counter every attachment sweep
//! pass mints its generation from (ADR 0067 §6).
//!
//! A generation is stamped on each condemnation the pass creates or adopts,
//! and a later pass adopts only rows an older generation left. The counter
//! only rises, so no two passes share a generation while its row stands. The
//! singleton flag is `INTEGER 1` on SQLite and `BOOLEAN TRUE` on PostgreSQL,
//! so it is bound rather than spelled, and the mint is an upsert: a catalog
//! whose seed row is missing starts the count again rather than refusing
//! every sweep.

/// The table's unprefixed name.
pub const TABLE: &str = "attachment_sweep_clock";

crate::statements! {
    /// `attachment_sweep_clock` statements both backends issue verbatim.
    pub struct SweepClockStatements @ "attachment_sweep_clock" {
        /// Mint the next sweep generation and read it back in one statement.
        /// `?1` is the singleton flag, bound as boolean `true`.
        mint_generation = "INSERT INTO attachment_sweep_clock (singleton, generation)
             VALUES (?1, 1)
             ON CONFLICT (singleton)
             DO UPDATE SET generation = attachment_sweep_clock.generation + 1
             RETURNING generation";
    }
}
