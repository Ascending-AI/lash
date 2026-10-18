//! The machine's per-site property caches (`compile::reads`): each read
//! site's slot, kept only while this machine lives. A restored machine
//! starts with every site empty.

use crate::compile::ReadSite;

/// Each site's cached slot plus one; zero is a site with none.
#[derive(Default)]
pub(crate) struct SiteCaches(Vec<u32>);

impl SiteCaches {
    /// The slot `site` last found its property at.
    #[inline]
    pub(crate) fn slot(&self, site: ReadSite) -> Option<usize> {
        if site.missing {
            return None;
        }
        let slot = *self.0.get(site.at.ix())?;
        slot.checked_sub(1).map(|slot| slot as usize)
    }

    /// Keeps the slot `site` found its property at, or none.
    #[inline]
    pub(crate) fn keep(&mut self, site: ReadSite, slot: Option<usize>) {
        if site.missing {
            return;
        }
        let slot = slot
            .and_then(|slot| u32::try_from(slot).ok())
            .and_then(|slot| slot.checked_add(1))
            .unwrap_or(0);
        let at = site.at.ix();
        if at >= self.0.len() {
            if slot == 0 {
                return;
            }
            self.0.resize(at + 1, 0);
        }
        self.0[at] = slot;
    }
}
