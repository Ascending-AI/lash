//! Property read sites. Each field read and each record index read is a
//! site ([`SiteIx`]) whose per-site monomorphic cache its machine keeps
//! (`docs/design/kernel-end-state.md` §3.4): the slot the last record it
//! read held the property at. A hit is the record's entry at that slot,
//! guarded by the entry's key being the read's name, so a record with
//! another layout, a deletion or a reinsertion simply misses. A cache
//! retains no guest value and is never part of a checkpoint; a restored
//! machine starts with none.

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Value};
use lash_kernel_exec::SiteIx;
use lash_kernel_state::arena::{Arena, text_key};

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

/// The value of `record` under the key whose text is `name` and whose
/// normal form `key` gives, the cached `slot` tried first, and the slot
/// the value was found at.
pub(crate) fn find(
    arena: &Arena,
    record: ArenaHandle,
    slot: Option<usize>,
    name: &str,
    key: impl FnOnce() -> Result<NormalKey, NativeError>,
) -> Result<(Option<Value>, Option<usize>), NativeError> {
    if let Some(slot) = slot {
        let entries = arena.entries(record)?;
        let at = slot * 2;
        if let Some(pair) = entries.get(at..at + 2)
            && keyed(arena, &pair[0], name)
        {
            return Ok((Some(pair[1]), Some(slot)));
        }
    }
    let found = arena.table_find(record, &key()?)?;
    Ok((found.map(|(_, value)| value), found.map(|(at, _)| at)))
}

/// Whether a record entry's key is the text `name`: the cache's guard.
#[inline]
fn keyed(arena: &Arena, key: &Value, name: &str) -> bool {
    match key {
        Value::Text(short) => short.as_str() == name,
        Value::LongText(_) => arena.semantics().text(key).ok() == Some(name),
        _ => false,
    }
}
