//! Per-site field-position hints. A hint retains no guest value and is
//! never part of a checkpoint. Every hit verifies the current row's name;
//! a record with another layout, deletion or reinsertion simply misses.

use std::sync::atomic::{AtomicU64, Ordering};

use lash_kernel_doc::heap::{FieldName, HeapFault};
use lash_kernel_doc::{EntityId, NativeHeap, Value};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::TableCursor;

use super::Layout;

pub(crate) struct ReadHint {
    position: AtomicU64,
    forced_miss: bool,
}

impl ReadHint {
    pub(super) fn new(layout: Layout) -> Option<Self> {
        layout.0.is_multiple_of(2).then(|| Self {
            position: AtomicU64::new(u64::MAX),
            forced_miss: layout.0 % 4 == 2,
        })
    }

    pub(crate) fn read(
        &self,
        heap: &PagedHeap,
        record: EntityId,
        field: FieldName<'_>,
    ) -> Result<Option<Value>, HeapFault> {
        // A table never assigns MAX: inserting a row checks next + 1.
        let position = if self.forced_miss {
            u64::MAX
        } else {
            self.position.load(Ordering::Relaxed)
        };
        let position = (self.forced_miss || position != u64::MAX).then_some(TableCursor(position));
        let (value, hint) = heap.record_get_hinted(record, field, position)?;
        if !self.forced_miss {
            self.position
                .store(hint.map_or(u64::MAX, |hint| hint.0), Ordering::Relaxed);
        }
        Ok(value)
    }
}

pub(crate) struct Field {
    pub(crate) name: String,
    hint: Option<ReadHint>,
}

impl Field {
    pub(super) fn new(name: &str, layout: Layout) -> Self {
        Self {
            name: name.to_owned(),
            hint: ReadHint::new(layout),
        }
    }

    pub(crate) fn read(
        &self,
        heap: &PagedHeap,
        record: EntityId,
    ) -> Result<Option<Value>, HeapFault> {
        let field = FieldName::Static(&self.name);
        match &self.hint {
            Some(hint) => hint.read(heap, record, field),
            None => heap.record_get(record, field),
        }
    }
}
