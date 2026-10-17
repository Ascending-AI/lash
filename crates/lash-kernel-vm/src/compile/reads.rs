//! Per-site field-position hints. A hint retains no guest value and is
//! never part of a checkpoint. Every hit verifies the current entry's
//! name; a record with another layout, deletion or reinsertion simply
//! misses.

use std::sync::atomic::{AtomicU64, Ordering};

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Value};
use lash_kernel_state::arena::{Arena, text_key};

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

    /// The value of field `name`, keyed `key`, of `record`.
    pub(crate) fn read(
        &self,
        arena: &Arena,
        record: ArenaHandle,
        key: &NormalKey,
        name: &str,
    ) -> Result<Option<Value>, NativeError> {
        let position = if self.forced_miss {
            u64::MAX
        } else {
            self.position.load(Ordering::Relaxed)
        };
        if position != u64::MAX {
            let entries = arena.entries(record)?;
            let at = position as usize * 2;
            if let Some(pair) = entries.get(at..at + 2)
                && arena.semantics().text(&pair[0]).ok() == Some(name)
            {
                return Ok(Some(pair[1]));
            }
        }
        let found = arena.table_find(record, key)?;
        if !self.forced_miss {
            self.position.store(
                found.map_or(u64::MAX, |(at, _)| at as u64),
                Ordering::Relaxed,
            );
        }
        Ok(found.map(|(_, value)| value))
    }
}

pub(crate) struct Field {
    pub(crate) name: String,
    /// The name's normal form, which the record's index keys it by.
    pub(crate) key: NormalKey,
    hint: Option<ReadHint>,
}

impl Field {
    pub(super) fn new(name: &str, layout: Layout) -> Self {
        Self {
            name: name.to_owned(),
            key: text_key(name),
            hint: ReadHint::new(layout),
        }
    }

    /// The value of this field of `record`, absent when it has none.
    pub(crate) fn read(&self, arena: &Arena, record: ArenaHandle) -> Result<Value, NativeError> {
        let value = match &self.hint {
            Some(hint) => hint.read(arena, record, &self.key, &self.name)?,
            None => arena.table_find(record, &self.key)?.map(|(_, value)| value),
        };
        Ok(value.unwrap_or(Value::Absent))
    }
}
