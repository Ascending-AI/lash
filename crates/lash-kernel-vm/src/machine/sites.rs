//! The machine's per-site property caches (`compile::reads`): each read
//! site's slot, kept only while this machine lives. A restored machine
//! starts with every site empty.

use crate::compile::{PropertySlot, ReadSite};

/// Each site's guarded short key and position.
#[derive(Default)]
pub(crate) struct SiteCaches(Vec<Option<PropertySlot>>);

impl SiteCaches {
    /// The slot `site` last found its property at.
    #[inline]
    pub(crate) fn slot(&self, site: ReadSite) -> Option<PropertySlot> {
        if site.missing {
            return None;
        }
        self.0.get(site.at.ix()).copied().flatten()
    }

    /// Keeps the slot `site` found its property at, or none.
    #[inline]
    pub(crate) fn keep(&mut self, site: ReadSite, slot: Option<PropertySlot>) {
        if site.missing {
            return;
        }
        let at = site.at.ix();
        if at >= self.0.len() {
            if slot.is_none() {
                return;
            }
            self.0.resize(at + 1, None);
        }
        self.0[at] = slot;
    }
}
