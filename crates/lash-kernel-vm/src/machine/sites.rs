//! The machine's per-site property caches (`compile::reads`): each read
//! site's slot, kept only while this machine lives. A restored machine
//! starts with every site empty.

use crate::compile::{PropertySlot, ReadSite};
use std::collections::BTreeMap;

/// Each site's guarded short key and position.
#[derive(Default)]
pub(crate) struct SiteCaches {
    slots: BTreeMap<usize, Option<PropertySlot>>,
}

impl SiteCaches {
    /// The slot `site` last found its property at.
    #[inline]
    pub(crate) fn slot(&self, site: ReadSite) -> Option<PropertySlot> {
        if site.missing {
            return None;
        }
        self.slots.get(&site.at.ix()).copied().flatten()
    }

    /// Keeps the slot `site` found its property at, or none.
    #[inline]
    pub(crate) fn keep(&mut self, site: ReadSite, slot: Option<PropertySlot>) {
        if site.missing {
            return;
        }
        self.keep_at(site.at.ix(), slot);
    }

    fn keep_at(&mut self, at: usize, slot: Option<PropertySlot>) {
        if slot.is_some() {
            self.slots.insert(at, slot);
        } else if let Some(stored) = self.slots.get_mut(&at) {
            *stored = None;
        }
    }
}
