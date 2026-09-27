//! The obligation column set (ADR 0109 §1.1), shared by every ledger that
//! owes the engine an effect.
//!
//! Each ledger's table module declares its own obligation statements — the
//! same operations over its own table and key — and implements
//! [`ObligationStatementSet`], so a backend drives every ledger through one
//! generic implementation that reads the statements it needs from
//! [`ObligationSql`]. The key columns always come last in a claim's and a
//! stalled listing's projection, in the table's key order.

use crate::Rendered;

/// One ledger's rendered obligation statements.
#[derive(Clone, Copy, Debug)]
pub struct ObligationSql<'a> {
    /// How many key columns the arm binds first and the projections end with.
    pub key_columns: usize,
    pub arm: &'a Rendered,
    pub select_due: &'a Rendered,
    pub claim_due_row: &'a Rendered,
    pub claim: &'a Rendered,
    pub settle_delivered: &'a Rendered,
    pub settle_retry: &'a Rendered,
    pub settle_stall: &'a Rendered,
    pub rearm: &'a Rendered,
    pub select_stalled: &'a Rendered,
    pub count_stalled: &'a Rendered,
    pub select_state: &'a Rendered,
}

/// A table module's obligation statement set.
pub trait ObligationStatementSet {
    /// The set's statements, by role.
    fn obligation_sql(&self) -> ObligationSql<'_>;
}
