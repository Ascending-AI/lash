//! One weighted residency for read pages and their decoded forms. Encoded
//! pages, decoded nodes and metadata share a single byte budget, enforced by
//! segmented LRU eviction: an entry enters on probation and is protected once
//! read again, so a one-pass scan evicts its own pages before the re-read
//! working set. Every entry is reproducible: a dropped page is fetched again
//! through the source and authenticated like any cold read, and a dropped
//! decoding is decoded again from such a page.
use super::super::{NameRef, SiteRef};
use super::{PageAnswer, Summary};
use lash_kernel_doc::heap::{FamilyId, PageId};
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Site;
use std::any::{Any, TypeId};
use std::collections::BTreeMap;

/// The shared bound on encoded and decoded residency.
pub(super) const BUDGET: usize = 64 << 20;
/// Re-read entries keep at most this share; the rest stays open to probation
/// so newly read pages can prove themselves before they displace the set.
const PROTECTED: usize = BUDGET / 5 * 4;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Slot {
    Page(PageId),
    Node(PageId, TypeId, Option<FamilyId>, bool),
    /// An interned name by its retained allocation's address and length.
    Name(usize, usize),
    Site(Site),
    Projected(PageId, u64, u64, u64),
}

pub(super) enum Held {
    Page(PageAnswer),
    Node {
        node: Box<dyn Any>,
        summary: Summary,
        encoded: usize,
    },
    Name {
        /// Retained so the key's address cannot name another name.
        _name: Name,
        value: NameRef,
    },
    Site(SiteRef),
    Projected(Name),
}

struct Entry {
    held: Held,
    weight: usize,
    tick: u64,
    protected: bool,
}

#[derive(Default)]
pub(super) struct Residency {
    entries: BTreeMap<Slot, Entry>,
    probation: BTreeMap<u64, Slot>,
    protected: BTreeMap<u64, Slot>,
    bytes: usize,
    protected_bytes: usize,
    clock: u64,
}

impl Residency {
    /// A held entry, marked as re-read.
    pub(super) fn get(&mut self, slot: &Slot) -> Option<&Held> {
        let tick = self.tick();
        let entry = self.entries.get_mut(slot)?;
        if entry.protected {
            self.protected.remove(&entry.tick);
        } else {
            self.probation.remove(&entry.tick);
            entry.protected = true;
            self.protected_bytes += entry.weight;
        }
        entry.tick = tick;
        self.protected.insert(tick, slot.clone());
        while self.protected_bytes > PROTECTED {
            let Some((_, demoted)) = self.protected.pop_first() else {
                break;
            };
            let tick = self.tick();
            let entry = self.entries.get_mut(&demoted)?;
            entry.protected = false;
            entry.tick = tick;
            self.protected_bytes -= entry.weight;
            self.probation.insert(tick, demoted);
        }
        self.entries.get(slot).map(|entry| &entry.held)
    }

    /// Holds `held` on probation, evicting least recently read entries,
    /// probation first, until the budget admits it. An entry larger than
    /// the whole budget is not held.
    pub(super) fn insert(&mut self, slot: Slot, held: Held, weight: usize) {
        if weight > BUDGET {
            return;
        }
        self.remove(&slot);
        while self.bytes + weight > BUDGET {
            let Some((_, victim)) = self
                .probation
                .pop_first()
                .or_else(|| self.protected.pop_first())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.bytes -= entry.weight;
                if entry.protected {
                    self.protected_bytes -= entry.weight;
                }
            }
        }
        let tick = self.tick();
        self.bytes += weight;
        self.probation.insert(tick, slot.clone());
        self.entries.insert(
            slot,
            Entry {
                held,
                weight,
                tick,
                protected: false,
            },
        );
    }

    fn remove(&mut self, slot: &Slot) {
        let Some(entry) = self.entries.remove(slot) else {
            return;
        };
        self.bytes -= entry.weight;
        if entry.protected {
            self.protected_bytes -= entry.weight;
            self.protected.remove(&entry.tick);
        } else {
            self.probation.remove(&entry.tick);
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}
