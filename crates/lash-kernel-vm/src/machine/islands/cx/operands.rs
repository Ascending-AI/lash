//! The operand primitives of the register program's heap context
//! ([`Cx`]): scalar and index reads, the positions a frame's variable
//! takes, and the keyed and element writes a statement holds until it
//! commits. What an instruction builds is installed as an ordinary
//! statement installs it, its growth held first from the nursery admitted
//! at entry (§3.5), so that no admission inside refuses. A miss after an
//! object was built leaves it garbage, held and released by count as any
//! garbage is.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body};

use lash_kernel_state::arena::{Key, VALUE_BYTES};

use super::Cx;
use crate::compile::{ReadSite, find_property, write_property};

/// What a store's operand is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Target,
    Index,
    Value,
}

/// Where a store writes in its target.
#[derive(Clone)]
pub(crate) enum Place {
    /// At the index its operands give.
    Index,
    /// The field of this name.
    Field(Value),
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
/// rather than from the arena ([`Cx::operate_on`]).
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

/// Field `name` of error `error`, as the reference reads it.
pub(super) fn error_field(cx: &Cx<'_>, error: ArenaHandle, name: &str) -> Option<Value> {
    match cx.arena.body(error).ok()? {
        Body::Error(parts) => Some(match name {
            "kind" => parts.kind,
            "message" => parts.message,
            "data" => parts.data,
            _ => Value::Absent,
        }),
        _ => None,
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
    cx: &mut Cx<'_>,
    site: Option<ReadSite>,
    target: Value,
    index: Value,
) -> Option<Value> {
    let arena = &*cx.arena;
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
            let slot = cx.sites.slot(site);
            let key = arena.key(&index).ok()?;
            let (found, at) = find_property(arena, record, slot, &key).ok()?;
            if at != slot {
                cx.sites.keep(site, at);
            }
            cx.keep_keyed(site, record);
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

/// A heap path's write to a guest object, checked as the reference's
/// member write checks it and held until its statement commits
/// ([`Cx::commit_write`]), its growth's capacity reserved out of the room. The
/// write is its operator's last evaluation and the operator its
/// statement's (`compile::regions`), so the statement evaluates nothing
/// after it that reads what it writes; a statement that misses drops it.
pub(crate) struct Held {
    pub(super) write: Prepared,
    pub(super) bound: u64,
}

/// A member write checked as the reference's checks it: what it writes
/// where, its growth not yet held.
pub(crate) enum Prepared {
    /// `value` at position `at` of `list`, appending at its length.
    List {
        list: ArenaHandle,
        at: u64,
        value: Value,
    },
    /// `written` under `key` of `table`.
    Table {
        table: ArenaHandle,
        key: Value,
        written: Value,
    },
    /// `key` out of `set`.
    Unset { set: ArenaHandle, key: Value },
}

/// Writes `value` at `place` of `target`, with `index` for an index, as
/// the reference's member write does, marking what it changes dirty, its
/// growth (a new member's value slots) admitted by the room first; `None`,
/// having written nothing, where the reference raises or must decode
/// first.
pub(crate) fn store(
    cx: &mut Cx<'_>,
    place: &Place,
    target: Value,
    index: Value,
    value: Value,
) -> Option<()> {
    if let Value::Record(record) = target
        && cx.keyed.is_some_and(|(read, _)| read == record)
        && keyed_store(cx, place, record, index, value)
    {
        return Some(());
    }
    let (write, _) = prepare(cx, place, target, index, value)?;
    apply(cx, write)
}

/// Writes `value` under a record's short key where the record's last
/// read through a caching site found that key, and the record is still
/// the version that read proved it in: the write [`prepare`] checks and
/// [`apply`] makes, its growth admitted as theirs, without finding the
/// key again. `false`, having written nothing, otherwise. Out of line,
/// so that the other targets' stores stay [`prepare`]'s and [`apply`]'s.
#[inline(never)]
fn keyed_store(
    cx: &mut Cx<'_>,
    place: &Place,
    record: ArenaHandle,
    index: Value,
    value: Value,
) -> bool {
    let Some((read, site)) = cx.keyed else {
        return false;
    };
    let key = match (place, index) {
        (Place::Field(Value::Text(name)), _) => *name,
        (Place::Index, Value::Text(key)) => key,
        _ => return false,
    };
    // With no cold chunk anywhere, no key is unresolved.
    record == read
        && !cx.arena.any_cold()
        && cx.fits(2 * VALUE_BYTES)
        && cx.sites.slot(site).is_some_and(|slot| {
            write_property(cx.arena, record, slot, &key, value).is_ok_and(|written| written)
        })
}

/// The write of `value` at `place` of `target`, with `index` for an
/// index, checked as the reference's member write checks it, and the most
/// capacity its growth holds, which the room admits; `None` where the
/// reference raises or must decode first.
#[inline(always)]
pub(super) fn prepare(
    cx: &Cx<'_>,
    place: &Place,
    target: Value,
    index: Value,
    value: Value,
) -> Option<(Prepared, u64)> {
    let (table, key, written) = match (target, place) {
        (Value::List(list), Place::Index) => {
            let length = cx.arena.len(list).ok()?;
            let at = position(&index).filter(|at| *at <= length)?;
            if !cx.fits(VALUE_BYTES) {
                return None;
            }
            return Some((Prepared::List { list, at, value }, VALUE_BYTES));
        }
        (Value::Record(record), Place::Field(name)) => (record, *name, value),
        (Value::Record(record), Place::Index)
            if matches!(index, Value::Text(_) | Value::LongText(_)) =>
        {
            (record, index, value)
        }
        (Value::Map(map), Place::Index) => {
            cx.arena.key(&index).ok()?;
            (map, index, value)
        }
        (Value::Set(set), Place::Index) => {
            cx.arena.key(&index).ok()?;
            match value {
                Value::Bool(true) => (set, index, Value::Absent),
                Value::Bool(false) => {
                    if cx.arena.unresolved(set, &index).ok()?.is_some() {
                        return None;
                    }
                    return Some((Prepared::Unset { set, key: index }, 0));
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let bound = 2 * VALUE_BYTES;
    if (cx.arena.any_cold() && cx.arena.unresolved(table, &key).ok()?.is_some()) || !cx.fits(bound)
    {
        return None;
    }
    Some((
        Prepared::Table {
            table,
            key,
            written,
        },
        bound,
    ))
}

/// Makes `write`, which [`prepare`] checked; `None` where the arena must
/// decode first. A record's short key that a read found still where it
/// was is written there, without finding it again.
#[inline(always)]
pub(super) fn apply(cx: &mut Cx<'_>, write: Prepared) -> Option<()> {
    match write {
        Prepared::List { list, at, value } => cx.arena.list_set(list, at, value).ok(),
        Prepared::Table {
            table,
            key,
            written,
        } => {
            if let (Some((record, site)), Value::Text(text)) = (cx.keyed, key)
                && record == table
                && let Some(slot) = cx.sites.slot(site)
                && write_property(cx.arena, table, slot, &text, written)
                    .is_ok_and(|written| written)
            {
                return Some(());
            }
            cx.arena.table_write(table, key, written).ok()
        }
        Prepared::Unset { set, key } => cx.arena.table_delete(set, &key).ok(),
    }
}
