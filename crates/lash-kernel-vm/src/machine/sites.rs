//! The machine's per-site property caches (`compile::reads`): each read
//! site's slot, kept only while this machine lives. A restored machine
//! starts with every site empty.

use crate::compile::{PropertySlot, ReadSite};

/// Each site's guarded short key and position.
#[derive(Default)]
pub(crate) struct SiteCaches {
    properties: Vec<Option<PropertySlot>>,
    guards: Vec<Option<PropertySlot>>,
    writes: Vec<Option<PropertySlot>>,
}

impl SiteCaches {
    /// The slot `site` last found its property at.
    #[inline]
    pub(crate) fn slot(&self, site: ReadSite) -> Option<PropertySlot> {
        if site.missing {
            return None;
        }
        self.properties.get(site.at.ix()).copied().flatten()
    }

    /// Keeps the slot `site` found its property at, or none.
    #[inline]
    pub(crate) fn keep(&mut self, site: ReadSite, slot: Option<PropertySlot>) {
        if site.missing {
            return;
        }
        let at = site.at.ix();
        if at >= self.properties.len() {
            if slot.is_none() {
                return;
            }
            self.properties.resize(at + 1, None);
        }
        self.properties[at] = slot;
    }

    #[inline]
    pub(crate) fn write(&self, statement: crate::compile::StmtId) -> Option<PropertySlot> {
        self.writes.get(statement.0 as usize).copied().flatten()
    }

    pub(crate) fn keep_write(
        &mut self,
        statement: crate::compile::StmtId,
        slot: Option<PropertySlot>,
    ) {
        let at = statement.0 as usize;
        if at >= self.writes.len() {
            if slot.is_none() {
                return;
            }
            self.writes.resize(at + 1, None);
        }
        self.writes[at] = slot;
    }

    #[inline]
    pub(crate) fn guard(&self, statement: crate::compile::StmtId) -> Option<PropertySlot> {
        self.guards.get(statement.0 as usize).copied().flatten()
    }

    pub(crate) fn keep_guard(
        &mut self,
        statement: crate::compile::StmtId,
        slot: Option<PropertySlot>,
    ) {
        let at = statement.0 as usize;
        if at >= self.guards.len() {
            if slot.is_none() {
                return;
            }
            self.guards.resize(at + 1, None);
        }
        self.guards[at] = slot;
    }
}
