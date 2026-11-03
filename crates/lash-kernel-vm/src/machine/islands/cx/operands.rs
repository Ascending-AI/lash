//! Canonical scalar and resident indexed reads.
use crate::compile::{ReadSite, find_property};
use lash_kernel_doc::Value;
use lash_kernel_state::arena::Key;

/// What a store's operand is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Target,
    Index,
    Value,
}

/// The values a region holds in registers and slots.
#[inline(always)]
pub(crate) fn scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
    )
}

/// Which of a call's operands are constructions of no members, by
/// position: a node's result whose measurements its kind proves
/// ([`crate::data::empty_measure`]), which the call's charge takes from it
/// rather than from the arena.
#[derive(Clone, Copy, Default)]
pub(crate) struct Fresh(u16);

impl Fresh {
    /// The operands whose positions `mask` sets.
    pub(crate) fn of(mask: u16) -> Self {
        Self(mask)
    }

    /// Whether the operand at `at` is a construction of no members.
    #[inline(always)]
    pub(crate) fn empty(self, at: usize) -> bool {
        at < 16 && self.0 & (1 << at) != 0
    }
}

/// The list position `index` is, as the reference reads it; `None` where
/// it raises.
pub(crate) fn position(index: &Value) -> Option<u64> {
    match index {
        Value::Int(word) => u64::try_from(*word).ok(),
        Value::Float(float) => {
            let float = float.get();
            (float.fract() == 0.0 && float >= 0.0 && float < usize::MAX as f64)
                .then_some(float as u64)
        }
        _ => None,
    }
}

/// What `target` holds at `index`, as the reference reads it: a record's
/// through `site`'s property cache, if it has one; `None` where the
/// reference raises or must decode first.
pub(crate) fn read_index(
    arena: &lash_kernel_state::arena::Arena,
    sites: &mut crate::machine::sites::SiteCaches,
    site: Option<ReadSite>,
    target: Value,
    index: Value,
) -> Option<Value> {
    match target {
        Value::Tuple(object) | Value::List(object) => arena.item(object, position(&index)?).ok()?,
        Value::Record(record) => {
            if !matches!(index, Value::Text(_) | Value::LongText(_)) {
                return None;
            }
            let Some(site) = site else {
                // A template's short key is already normalized. Read it
                // from one resident view, without building native Args.
                let found = match index {
                    Value::Text(text) => arena
                        .table_find(record, &Key::text(&text))
                        .ok()?
                        .map(|(_, value)| value),
                    _ => arena.table_get(record, &index).ok()?,
                };
                return Some(found.unwrap_or(Value::Absent));
            };
            let slot = sites.slot(site);
            let key = arena.key(&index).ok()?;
            let (found, at) = find_property(arena, record, slot, &key).ok()?;
            if at != slot {
                sites.keep(site, at);
            }
            Some(found.unwrap_or(Value::Absent))
        }
        Value::Map(map) => {
            if arena.unresolved(map, &index).ok()?.is_some() {
                return None;
            }
            arena.table_get(map, &index).ok()?
        }
        Value::Set(set) => {
            if arena.unresolved(set, &index).ok()?.is_some() {
                return None;
            }
            Some(Value::Bool(arena.table_get(set, &index).ok()?.is_some()))
        }
        _ => None,
    }
}
