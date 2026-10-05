//! Activity-identity dedup, range-aware (FIG-3753, FIG-5098).
//!
//! A replayed shift region or a journaled step re-executed after a mid-run
//! suspension republishes the activities its first attempt delivered, under
//! the ids the same observations derive: `{key}#{ordinal}` for one delta or
//! event, `{key}#{first}..{last}` for a frame of coalesced deltas (see
//! `turn_observer::framing`). The redrive may frame its deltas differently:
//! one frame where the first attempt published three, or the originals
//! unmerged. So the buffer dedups by the ordinals a draft covers, not by its
//! literal id.
//!
//! Per replay key, the buffer keeps the ordinal ranges it delivered. A
//! publisher yields a key's ordinals in ascending order and cuts a frame at
//! every other activity of its key, so the ordinals from a key's first
//! retained range through its last are all delivered or never were
//! activities. Against that span, a draft is:
//!
//! - **delivered** when its range lies inside: it is dropped, so a redrive
//!   of unmerged originals or of a different merge adds no text twice;
//! - **fresh** when its range lies wholly after (or wholly before, once the
//!   window trimmed it): it is published;
//! - **overlapping** otherwise: part of its text was delivered and part was
//!   not. A frame carries its deltas' text concatenated, with no boundaries,
//!   so the undelivered suffix cannot be cut out of it. Publishing it whole
//!   would duplicate the delivered part; dropping it would lose the rest
//!   silently. The store answers with a gap instead: the session's replay
//!   continuity is invalidated, so every cursor and subscription into it
//!   reloads the authoritative snapshot, and the draft is not published.
//!
//! An id no observation names is opaque and deduplicates exactly.

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;

use crate::TurnActivityId;

/// How a draft's activity identity meets what the buffer already claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Claim {
    Fresh,
    Delivered,
    Overlapping,
}

/// Activity identities appended to a session buffer, with the live position
/// each holds there, so trimming an event releases exactly its identity.
#[derive(Debug, Default)]
pub(super) struct DeliveredActivities {
    opaque: HashMap<TurnActivityId, u64>,
    /// Per replay key, the delivered ranges by first ordinal: last ordinal
    /// and position.
    ranges: HashMap<String, BTreeMap<u32, (u32, u64)>>,
}

impl DeliveredActivities {
    pub(super) fn insert(&mut self, id: &TurnActivityId, position: u64) {
        match id.observed_span() {
            Some((key, ordinals)) => {
                self.ranges
                    .entry(key.to_owned())
                    .or_default()
                    .insert(*ordinals.start(), (*ordinals.end(), position));
            }
            None => {
                self.opaque.insert(id.clone(), position);
            }
        }
    }

    /// Release `id` when the event at `position` still holds it.
    pub(super) fn remove(&mut self, id: &TurnActivityId, position: u64) {
        match id.observed_span() {
            Some((key, ordinals)) => {
                let Some(ranges) = self.ranges.get_mut(key) else {
                    return;
                };
                if ranges.get(ordinals.start()) == Some(&(*ordinals.end(), position)) {
                    ranges.remove(ordinals.start());
                }
                if ranges.is_empty() {
                    self.ranges.remove(key);
                }
            }
            None => {
                if self.opaque.get(id) == Some(&position) {
                    self.opaque.remove(id);
                }
            }
        }
    }

    /// The span of ordinals this buffer delivered under `key`.
    fn span(&self, key: &str) -> Option<RangeInclusive<u32>> {
        let ranges = self.ranges.get(key)?;
        let (first, _) = ranges.first_key_value()?;
        let (_, (last, _)) = ranges.last_key_value()?;
        Some(*first..=*last)
    }

    /// How `id` meets the delivered identities and the `claimed` ones (in
    /// flight, or earlier in the same batch).
    pub(super) fn claim<'a>(
        &self,
        id: &TurnActivityId,
        claimed: impl IntoIterator<Item = &'a TurnActivityId>,
    ) -> Claim {
        let Some((key, ordinals)) = id.observed_span() else {
            let delivered =
                self.opaque.contains_key(id) || claimed.into_iter().any(|claimed| claimed == id);
            return if delivered {
                Claim::Delivered
            } else {
                Claim::Fresh
            };
        };
        let span = claimed
            .into_iter()
            .filter_map(TurnActivityId::observed_span)
            .filter(|(claimed_key, _)| *claimed_key == key)
            .map(|(_, claimed)| claimed)
            .chain(self.span(key))
            .reduce(|span, more| {
                (*span.start()).min(*more.start())..=(*span.end()).max(*more.end())
            });
        let Some(span) = span else {
            return Claim::Fresh;
        };
        if ordinals.start() > span.end() || ordinals.end() < span.start() {
            Claim::Fresh
        } else if ordinals.start() >= span.start() && ordinals.end() <= span.end() {
            Claim::Delivered
        } else {
            Claim::Overlapping
        }
    }
}
