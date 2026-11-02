//! The machine's per-site property caches (`compile::reads`): each read
//! site's slot, kept only while this machine lives. A restored machine
//! starts with every site empty.

use crate::compile::{PropertySlot, ReadSite};
use std::collections::BTreeMap;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum CachedSite {
    Read(usize),
    Guard(usize),
    Write(usize),
}

/// Each site's guarded short key and position.
#[derive(Default)]
pub(crate) struct SiteCaches {
    slots: BTreeMap<CachedSite, Option<PropertySlot>>,
}

impl SiteCaches {
    /// The slot `site` last found its property at.
    #[inline]
    pub(crate) fn slot(&self, site: ReadSite) -> Option<PropertySlot> {
        if site.missing {
            return None;
        }
        self.slots
            .get(&CachedSite::Read(site.at.ix()))
            .copied()
            .flatten()
    }

    /// Keeps the slot `site` found its property at, or none.
    #[inline]
    pub(crate) fn keep(&mut self, site: ReadSite, slot: Option<PropertySlot>) {
        if site.missing {
            return;
        }
        self.keep_at(CachedSite::Read(site.at.ix()), slot);
    }

    #[inline]
    pub(crate) fn write(&self, statement: crate::compile::StmtId) -> Option<PropertySlot> {
        self.slots
            .get(&CachedSite::Write(statement.0 as usize))
            .copied()
            .flatten()
    }

    pub(crate) fn keep_write(
        &mut self,
        statement: crate::compile::StmtId,
        slot: Option<PropertySlot>,
    ) {
        self.keep_at(CachedSite::Write(statement.0 as usize), slot);
    }

    #[inline]
    pub(crate) fn guard(&self, statement: crate::compile::StmtId) -> Option<PropertySlot> {
        self.slots
            .get(&CachedSite::Guard(statement.0 as usize))
            .copied()
            .flatten()
    }

    pub(crate) fn keep_guard(
        &mut self,
        statement: crate::compile::StmtId,
        slot: Option<PropertySlot>,
    ) {
        self.keep_at(CachedSite::Guard(statement.0 as usize), slot);
    }

    fn keep_at(&mut self, at: CachedSite, slot: Option<PropertySlot>) {
        if slot.is_some() {
            self.slots.insert(at, slot);
        } else if let Some(stored) = self.slots.get_mut(&at) {
            *stored = None;
        }
    }
}
