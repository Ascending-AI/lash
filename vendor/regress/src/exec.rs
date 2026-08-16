// Copyright (c) 2020 ridiculous_fish; https://github.com/ridiculousfish/regress @ 7e64ad5e6807b5503e5cc97a79e0f129b23c556b; MIT licensed; modified: fuel/step-budget instrumentation and anchored matching API.
//! Execution engine bits.

use crate::api::Match;
use crate::insn::CompiledRegex;
use crate::position::PositionType;

/// Deterministic execution failure from a fuel-limited match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchError {
    /// The matcher consumed every configured bytecode/backtrack step.
    Exhausted,
}

impl core::fmt::Display for MatchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exhausted => f.write_str("regular expression execution fuel exhausted"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for MatchError {}

/// A trait for finding the next match in a regex.
/// This is broken out from Executor to avoid needing to thread lifetimes
/// around.
pub trait MatchProducer: core::fmt::Debug {
    /// The position type of our indexer.
    type Position: PositionType;

    /// \return an initial position for the given start offset.
    fn initial_position(&self, offset: usize) -> Option<Self::Position>;

    /// Attempt to match at the given location.
    /// \return either the Match and the position to start looking for the next
    /// match, or None on failure.
    fn next_match(
        &mut self,
        pos: Self::Position,
        next_start: &mut Option<Self::Position>,
    ) -> Option<Match>;
}

/// A match producer which reports deterministic fuel exhaustion.
pub trait FallibleMatchProducer: MatchProducer {
    /// Attempt to produce one match without hiding fuel exhaustion as no-match.
    fn try_next_match(
        &mut self,
        pos: Self::Position,
        next_start: &mut Option<Self::Position>,
    ) -> Result<Option<Match>, MatchError>;

    /// Attempt one match at exactly `pos`, without searching later positions.
    fn try_next_match_anchored(
        &mut self,
        pos: Self::Position,
        next_start: &mut Option<Self::Position>,
    ) -> Result<Option<Match>, MatchError>;
}

/// A trait for executing a regex.
pub trait Executor<'r, 't>: MatchProducer {
    /// The ASCII variant.
    type AsAscii: Executor<'r, 't>;

    /// Construct a new Executor.
    fn new(re: &'r CompiledRegex, text: &'t str) -> Self;
}

/// A struct which enables iteration over matches.
#[derive(Debug)]
pub struct Matches<Producer: MatchProducer> {
    mp: Producer,
    position: Option<Producer::Position>,
}

impl<Producer: MatchProducer> Matches<Producer> {
    pub fn new(mp: Producer, start: usize) -> Self {
        let position = mp.initial_position(start);
        Matches { mp, position }
    }
}

impl<Producer: MatchProducer> Iterator for Matches<Producer> {
    type Item = Match;
    fn next(&mut self) -> Option<Self::Item> {
        let pos = self.position?;
        self.mp.next_match(pos, &mut self.position)
    }
}

/// A fuel-limited iterator over matches.
#[derive(Debug)]
pub struct TryMatches<Producer: FallibleMatchProducer> {
    mp: Producer,
    position: Option<Producer::Position>,
    anchored: bool,
}

impl<Producer: FallibleMatchProducer> TryMatches<Producer> {
    pub fn new(mp: Producer, start: usize) -> Self {
        let position = mp.initial_position(start);
        Self {
            mp,
            position,
            anchored: false,
        }
    }

    pub fn new_anchored(mp: Producer, start: usize) -> Self {
        let position = mp.initial_position(start);
        Self {
            mp,
            position,
            anchored: true,
        }
    }
}

impl<Producer: FallibleMatchProducer> Iterator for TryMatches<Producer> {
    type Item = Result<Match, MatchError>;

    fn next(&mut self) -> Option<Self::Item> {
        let pos = self.position?;
        let found = if self.anchored {
            self.mp.try_next_match_anchored(pos, &mut self.position)
        } else {
            self.mp.try_next_match(pos, &mut self.position)
        };
        match found {
            Ok(Some(found)) => Some(Ok(found)),
            Ok(None) => {
                self.position = None;
                None
            }
            Err(error) => {
                self.position = None;
                Some(Err(error))
            }
        }
    }
}
