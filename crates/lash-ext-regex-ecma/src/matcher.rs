//! Driving a compiled program over an input, with every matcher step spent
//! on the call's work counter.

use std::ops::{ControlFlow, Range};

use lash_kernel_doc::{GuardExceeded, NativeError, WorkCounter};
use lash_regress::{Match, MeteredMatches, Regex};

/// One match, as ranges of the input's UTF-16 code units.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(crate) range: Range<usize>,
    /// One per capturing group; `None` where the group took no part.
    pub(crate) captures: Vec<Option<Range<usize>>>,
    /// The named groups, each name once, in pattern order.
    pub(crate) named: Vec<(String, Option<Range<usize>>)>,
}

impl From<Match> for Found {
    fn from(found: Match) -> Self {
        let named = found
            .named_groups()
            .map(|(name, range)| (name.to_string(), range))
            .collect();
        Self {
            range: found.range,
            captures: found.captures,
            named,
        }
    }
}

/// A compiled program over one input.
pub(crate) struct Search<'a> {
    pub(crate) program: &'a Regex,
    pub(crate) units: &'a [u16],
    /// The `u` flag: match by code point, not by code unit.
    pub(crate) unicode: bool,
}

impl Search<'_> {
    /// The first match at or after `start`, or exactly at `start` when
    /// `sticky`.
    pub(crate) fn first(
        &self,
        start: usize,
        sticky: bool,
        counter: &mut WorkCounter,
    ) -> Result<Option<Found>, NativeError> {
        let mut first = None;
        self.each(start, sticky, counter, |found| {
            first = Some(found);
            ControlFlow::Break(())
        })?;
        Ok(first)
    }

    /// Visits successive matches from `start` until the visitor breaks.
    /// After an empty match the search moves on by one code unit, or one
    /// code point under `u`. When `sticky`, each match begins exactly where
    /// the search stands and the first gap ends the visit.
    pub(crate) fn each(
        &self,
        start: usize,
        sticky: bool,
        counter: &mut WorkCounter,
        mut visit: impl FnMut(Found) -> ControlFlow<()>,
    ) -> Result<(), NativeError> {
        let fuel = counter
            .limit()
            .map_or(u64::MAX, |limit| limit.saturating_sub(counter.spent()));
        let mut expected = start;
        let step = |found: Match| {
            let found = Found::from(found);
            if sticky && found.range.start != expected {
                return ControlFlow::Break(());
            }
            expected = if found.range.is_empty() {
                advance_index(self.units, found.range.end, self.unicode)
            } else {
                found.range.end
            };
            visit(found)
        };
        let (program, units) = (self.program, self.units);
        match (self.unicode, sticky) {
            (true, true) => drive(
                program.try_find_from_utf16_anchored(units, start, fuel),
                counter,
                step,
            ),
            (true, false) => drive(
                program.try_find_from_utf16(units, start, fuel),
                counter,
                step,
            ),
            (false, true) => drive(
                program.try_find_from_ucs2_anchored(units, start, fuel),
                counter,
                step,
            ),
            (false, false) => drive(
                program.try_find_from_ucs2(units, start, fuel),
                counter,
                step,
            ),
        }
    }
}

/// Runs a metered search and spends what it consumed.
///
/// The matcher was granted exactly what the counter had left, so a search
/// that ran out stopped at the step that would pass the limit: the count is
/// the matcher's own, of a program that is a function of the pattern and
/// flags, and nothing about the engine's caches enters it.
fn drive<M: MeteredMatches>(
    mut matches: M,
    counter: &mut WorkCounter,
    mut visit: impl FnMut(Match) -> ControlFlow<()>,
) -> Result<(), NativeError> {
    let mut exhausted = false;
    loop {
        match matches.next() {
            None => break,
            Some(Ok(found)) => {
                if visit(found).is_break() {
                    break;
                }
            }
            Some(Err(_)) => {
                exhausted = true;
                break;
            }
        }
    }
    counter.spend(matches.consumed_fuel())?;
    if exhausted {
        // The step the matcher was refused.
        counter.spend(1)?;
        return Err(NativeError::Guard(GuardExceeded {
            limit: counter.limit().unwrap_or(u64::MAX),
        }));
    }
    Ok(())
}

/// ECMAScript's `AdvanceStringIndex`: the index after `index`, past a whole
/// surrogate pair under `u`.
pub(crate) fn advance_index(units: &[u16], index: usize, unicode: bool) -> usize {
    if unicode
        && let (Some(first), Some(second)) = (units.get(index), units.get(index + 1))
        && (0xd800..=0xdbff).contains(first)
        && (0xdc00..=0xdfff).contains(second)
    {
        index + 2
    } else {
        index.saturating_add(1)
    }
}
