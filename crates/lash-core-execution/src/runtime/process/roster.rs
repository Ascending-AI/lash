//! Bounded fleet discovery and its durable change fence.

use super::{ProcessChangeCursor, ProcessId, ProcessListFilter, ProcessRecord};

/// Maximum matching records in a roster page, excluding its single lookahead row.
pub const MAX_PROCESS_ROSTER_PAGE_SIZE: usize = 256;

/// The verified durable change high water and oldest resumable position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessChangeBounds {
    pub current: ProcessChangeCursor,
    pub retained_after: ProcessChangeCursor,
}

/// A store-bound keyset continuation. Pass it unchanged with the same filter.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRosterCursor {
    store: String,
    filter: ProcessListFilter,
    after: ProcessId,
    through: ProcessId,
    change_cursor: ProcessChangeCursor,
}

impl ProcessRosterCursor {
    /// Constructs a continuation for process-store implementors.
    pub fn new(
        store: String,
        filter: ProcessListFilter,
        after: ProcessId,
        through: ProcessId,
        change_cursor: ProcessChangeCursor,
    ) -> Self {
        Self {
            store,
            filter,
            after,
            through,
            change_cursor,
        }
    }

    pub fn after(&self) -> &ProcessId {
        &self.after
    }
    pub fn through(&self) -> &ProcessId {
        &self.through
    }
    pub fn change_cursor(&self) -> ProcessChangeCursor {
        self.change_cursor
    }

    /// Validates the issuing store, selection and retained scan fence.
    pub fn validate(
        &self,
        store: &str,
        filter: &ProcessListFilter,
        bounds: ProcessChangeBounds,
    ) -> Result<(), crate::PluginError> {
        if self.store != store {
            return Err(crate::PluginError::ProcessRegistryCursorBackendMismatch {
                expected: store.to_owned(),
                actual: self.store.clone(),
            });
        }
        if &self.filter != filter {
            return Err(crate::PluginError::ProcessRosterFilterMismatch {});
        }
        if self.change_cursor.store_sequence() < bounds.retained_after.store_sequence() {
            return Err(crate::PluginError::ProcessChangeCursorPruned {
                requested_cursor: self.change_cursor,
                tombstone_compaction_horizon: bounds.retained_after,
            });
        }
        Ok(())
    }
}

/// Store-side roster page. Enumeration is followed by changes after `change_cursor`.
/// The selection is applied before the keyset limit, including its lookahead.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRosterRecords {
    pub records: Vec<ProcessRecord>,
    pub continuation: Option<ProcessRosterCursor>,
    pub change_cursor: ProcessChangeCursor,
    pub verified_through: ProcessChangeCursor,
}

impl ProcessRosterRecords {
    /// Builds a page from a bounded keyset read of at most `limit + 1` matching records.
    pub fn from_candidates(
        store: String,
        filter: &ProcessListFilter,
        limit: usize,
        cursor: Option<&ProcessRosterCursor>,
        through: Option<ProcessId>,
        bounds: ProcessChangeBounds,
        mut candidates: Vec<ProcessRecord>,
    ) -> Self {
        debug_assert!(
            candidates
                .iter()
                .all(|record| filter.matches_record(record))
        );
        let change_cursor = cursor.map_or(bounds.current, ProcessRosterCursor::change_cursor);
        let more = candidates.len() > limit;
        candidates.truncate(limit);
        let continuation = if more {
            candidates.last().zip(through).map(|(last, through)| {
                ProcessRosterCursor::new(
                    store,
                    filter.clone(),
                    last.id.clone(),
                    through,
                    change_cursor,
                )
            })
        } else {
            None
        };
        Self {
            records: candidates,
            continuation,
            change_cursor,
            verified_through: bounds.current,
        }
    }
}
