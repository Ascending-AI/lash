//! Property sites cache a short key and its full chunk position behind the
//! arena object's identity and body revision. Every body edit invalidates the
//! guard. A changed object or shape verifies the entry's complete key before
//! reusing its position. Caches are process-local and empty on restore.

use std::borrow::Cow;

use lash_kernel_doc::arena::{ArenaHandle, ShortText};
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Value};
use lash_kernel_exec::SiteIx;
use lash_kernel_state::arena::{Arena, Place, text_key};

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
    /// The name's normal form, which the record's index keys it by.
    pub(crate) key: NormalKey,
    /// The read's site, when it is one that caches.
    pub(crate) site: Option<ReadSite>,
}

impl Field {
    pub(super) fn new(name: &str, site: u32, layout: Layout) -> Self {
        Self {
            name: name.to_owned(),
            key: text_key(name),
            site: ReadSite::new(site, layout),
        }
    }
}

/// A proved short key and physical position in one version of an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PropertySlot {
    version: (u64, u64),
    place: Place,
    key: [u8; ShortText::MAX],
    len: u8,
}

/// Reads through a proved position without normalizing or hashing a key.
/// On a changed object/shape the full entry key must match; otherwise the
/// canonical index supplies the fallback, by the key `key` gives: one the
/// caller holds is lent, not copied. Long keys use that index always.
pub(crate) fn find<'k>(
    arena: &Arena,
    record: ArenaHandle,
    slot: Option<PropertySlot>,
    name: &[u8],
    key: impl FnOnce() -> Result<Cow<'k, NormalKey>, NativeError>,
) -> Result<(Option<Value>, Option<PropertySlot>), NativeError> {
    let version = arena.table_version(record)?;
    if let Some(slot) = slot
        && &slot.key[..usize::from(slot.len)] == name
        && let Some(pair) = arena.table_at(record, slot.place)?
        && (Some(slot.version) == version || keyed(&pair[0], name))
    {
        return Ok((
            Some(pair[1]),
            version.map(|version| PropertySlot { version, ..slot }),
        ));
    }
    let found = arena.table_find(record, &*key()?)?;
    Ok((
        found.map(|(_, value)| value),
        found.and_then(|(place, _)| {
            if name.len() > ShortText::MAX {
                return None;
            }
            let mut key = [0; ShortText::MAX];
            key[..name.len()].copy_from_slice(name);
            Some(PropertySlot {
                version: version?,
                place,
                key,
                len: name.len() as u8,
            })
        }),
    ))
}

/// Short text is already validated UTF-8; equality needs only its bytes.
#[inline]
fn keyed(key: &Value, name: &[u8]) -> bool {
    matches!(key, Value::Text(short) if short.as_bytes() == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_doc::Datum;
    use lash_kernel_doc::arena::{Body, Table};
    use lash_kernel_state::arena::Bench;

    /// PLAN-INDEP: a proved position skips normalization only for its object,
    /// key and body revision. Insert, delete and kind changes revoke the proof.
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
        let (_, slot) = find(arena, record, None, b"a", || Ok(Cow::Owned(text_key("a")))).unwrap();
        let (hot, same) = find(arena, record, slot, b"a", || {
            panic!("a proved key is already normalized")
        })
        .unwrap();
        assert!(matches!(hot, Some(Value::Int(1))));
        assert_eq!(same, slot);
        let version = arena.table_version(record).unwrap();
        let c = arena.text("c");
        arena.table_write(record, c, Value::Int(3)).unwrap();
        assert_ne!(arena.table_version(record).unwrap(), version);
        let a = arena.text("a");
        arena.table_delete(record, &a).unwrap();
        let (removed, _) =
            find(arena, record, slot, b"a", || Ok(Cow::Owned(text_key("a")))).unwrap();
        assert!(removed.is_none(), "the old position now holds b");
        arena.table_write(record, a, Value::Int(4)).unwrap();
        assert_eq!(
            arena
                .table_after(record, Some(2))
                .unwrap()
                .map(|(seq, _)| seq),
            Some(3)
        );
        let (inserted, slot) =
            find(arena, record, slot, b"a", || Ok(Cow::Owned(text_key("a")))).unwrap();
        assert!(matches!(inserted, Some(Value::Int(4))));
        let version = arena.table_version(record).unwrap();
        let (body, _) = arena.parts_mut(record).unwrap();
        let Body::Record(table) = std::mem::replace(body, Body::Cell(Value::Absent)) else {
            panic!("record")
        };
        *body = Body::Map(table);
        assert_ne!(arena.table_version(record).unwrap(), version);
        let (changed_kind, slot) =
            find(arena, record, slot, b"a", || Ok(Cow::Owned(text_key("a")))).unwrap();
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
        let (replaced, _) =
            find(arena, record, slot, b"a", || Ok(Cow::Owned(text_key("a")))).unwrap();
        assert!(
            matches!(replaced, Some(Value::Int(7))),
            "a decoded body revokes its old position proof"
        );
    }
}
