//! Property sites cache a short key and its full chunk position, or its
//! absence, behind the arena object's identity and layout revision. An
//! edit that adds, removes or moves an entry invalidates the guard; a value
//! written over an entry keeps it. A changed object or shape verifies the
//! entry's complete key before reusing its position. Caches are
//! process-local and empty on restore.

use lash_kernel_doc::arena::{ArenaHandle, ShortText};
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Value};

use lash_kernel_exec::SiteIx;
pub(crate) use lash_kernel_state::arena::PropertySlot;
use lash_kernel_state::arena::{Arena, Key, text_key};

use super::Layout;

/// A read site under a layout that caches: odd layouts read through the
/// record's index always, and layouts 2 mod 4 consult the cache and miss.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadSite {
    pub(crate) at: SiteIx,
    pub(crate) missing: bool,
}

impl ReadSite {
    #[inline]
    pub(crate) fn new(at: u32, layout: Layout) -> Option<Self> {
        layout.0.is_multiple_of(2).then_some(Self {
            at: SiteIx(at),
            missing: layout.0 % 4 == 2,
        })
    }
}

/// A field name a read, a write or a removal names.
pub(crate) struct Field {
    pub(crate) name: String,
    /// The name's key, which the record's index keys it by.
    pub(crate) key: Key,
    /// The name's normal form, which the heap's key index keys it by.
    pub(crate) normal: NormalKey,
    /// The read's site, when it is one that caches.
    pub(crate) site: Option<ReadSite>,
}

impl Field {
    pub(super) fn new(name: &str, site: u32, layout: Layout) -> Self {
        Self {
            name: name.to_owned(),
            key: Key::of_text(name),
            normal: text_key(name),
            site: ReadSite::new(site, layout),
        }
    }
}

/// Reads `key` through a proved position, or a proved absence, without
/// hashing it. On a changed object/shape the full entry key must match;
/// otherwise the record's index supplies the fallback. Long keys use that
/// index always.
#[inline]
pub(crate) fn find(
    arena: &Arena,
    record: ArenaHandle,
    slot: Option<PropertySlot>,
    key: &Key,
) -> Result<(Option<Value>, Option<PropertySlot>), NativeError> {
    arena.table_read(record, slot, key)
}

/// Writes `value` under the short text `key` of `record` at the position
/// `slot` proved, where the record is still the version the proof names
/// and the entry there is keyed `key`: the write `table_write` makes
/// there, without hashing the key again. `false`, having written nothing,
/// where the proof is of an absence, of another key, or of an earlier
/// version.
#[inline]
pub(crate) fn write(
    arena: &mut Arena,
    record: ArenaHandle,
    slot: PropertySlot,
    key: &ShortText,
    value: Value,
) -> Result<bool, NativeError> {
    slot.write(arena, record, key, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_doc::Datum;
    use lash_kernel_doc::arena::{Body, Table};
    use lash_kernel_state::arena::Bench;

    /// PLAN-INDEP: a proved position or absence skips normalization only
    /// for its object, key and layout revision. Insert, delete and kind
    /// changes revoke the proof; a value written over an entry keeps it.
    #[test]
    fn property_position_proofs_expire_on_body_edits() {
        let mut bench = Bench::new();
        let Value::Record(record) = bench
            .build(&Datum::Record(vec![
                ("a".into(), Datum::Int(1.into())),
                ("b".into(), Datum::Int(2.into())),
            ]))
            .unwrap()
        else {
            panic!("record")
        };
        let arena = bench.arena_mut();
        let (_, slot) = find(arena, record, None, &Key::of_text("a")).unwrap();
        let (hot, same) = find(arena, record, slot, &Key::of_text("a")).unwrap();
        assert!(matches!(hot, Some(Value::Int(1))));
        assert_eq!(same, slot);
        let version = arena.table_version(record).unwrap();
        let a = arena.text("a");
        arena.table_write(record, a, Value::Int(5)).unwrap();
        assert_eq!(arena.table_version(record).unwrap(), version);
        let (written, same) = find(arena, record, slot, &Key::of_text("a")).unwrap();
        assert!(matches!(written, Some(Value::Int(5))));
        assert_eq!(same, slot);
        let (absent, missing) = find(arena, record, None, &Key::of_text("c")).unwrap();
        assert!(absent.is_none() && missing.is_some());
        let (absent, same) = find(arena, record, missing, &Key::of_text("c")).unwrap();
        assert!(absent.is_none());
        assert_eq!(same, missing);
        let c = arena.text("c");
        arena.table_write(record, c, Value::Int(3)).unwrap();
        assert_ne!(arena.table_version(record).unwrap(), version);
        let (inserted, _) = find(arena, record, missing, &Key::of_text("c")).unwrap();
        assert!(
            matches!(inserted, Some(Value::Int(3))),
            "an insert revokes a proved absence"
        );
        arena.table_delete(record, &a).unwrap();
        let (removed, _) = find(arena, record, slot, &Key::of_text("a")).unwrap();
        assert!(removed.is_none(), "the old position now holds b");
        arena.table_write(record, a, Value::Int(4)).unwrap();
        assert_eq!(
            arena
                .table_after(record, Some(2))
                .unwrap()
                .map(|(seq, _)| seq),
            Some(3)
        );
        let (inserted, slot) = find(arena, record, slot, &Key::of_text("a")).unwrap();
        assert!(matches!(inserted, Some(Value::Int(4))));
        let version = arena.table_version(record).unwrap();
        let (body, _) = arena.parts_mut(record).unwrap();
        let Body::Record(table) = std::mem::replace(body, Body::Cell(Value::Absent)) else {
            panic!("record")
        };
        *body = Body::Map(table);
        assert_ne!(arena.table_version(record).unwrap(), version);
        let (changed_kind, slot) = find(arena, record, slot, &Key::of_text("a")).unwrap();
        assert!(matches!(changed_kind, Some(Value::Int(4))));

        // A decoded replacement bypasses parts_mut and can move a position.
        arena.chill(record).unwrap().unwrap();
        let b = arena.text("b");
        arena
            .materialized(
                record,
                Body::Record(Table::whole(vec![
                    a,
                    Value::Int(7),
                    b,
                    Value::Int(2),
                    c,
                    Value::Int(3),
                ])),
            )
            .unwrap();
        let (replaced, _) = find(arena, record, slot, &Key::of_text("a")).unwrap();
        assert!(
            matches!(replaced, Some(Value::Int(7))),
            "a decoded body revokes its old position proof"
        );
    }
}
