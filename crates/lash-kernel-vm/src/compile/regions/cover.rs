//! An allocating plan's statement admission by runs
//! (`docs/design/kernel-regions.md` §2.5): each statement whose most is
//! bounded starts the straight-line run of such statements that follows it
//! in its block, and a region admits a run once, by the sum of its
//! statements' mosts, where the bound admits that sum whole.

use super::nodes::{Commit, CompiledBody, CompiledHead, CompiledStep};

/// A straight-line run of an allocating plan's statements: the statement
/// that starts it and those after it in its block, up to an `if` or a
/// `continue`, which ends it, or a statement whose most is unbounded or
/// that makes a guest call, which it stops before. A region admits the run
/// once, against the sum of its statements' mosts, where the bound admits
/// the sum: while each of its statements charges within its own most, every
/// statement's own admission passes, so none needs testing (FIG-7001).
#[derive(Clone, Copy)]
pub(crate) struct Cover {
    /// The most the run's statements charge together.
    pub(crate) most: u64,
    /// How many statements it holds, the one that starts it first.
    pub(crate) covers: u32,
}

impl Commit {
    /// The run that a statement committing this, followed by `next`'s run,
    /// starts; `ends` where the statement ends any run it is in.
    fn cover(&self, next: Option<Cover>, ends: bool) -> Option<Cover> {
        if self.guest.is_some() {
            return None;
        }
        let most = self.max?;
        Some(match next.filter(|_| !ends) {
            Some(next) => Cover {
                most: most.checked_add(next.most)?,
                covers: next.covers.checked_add(1)?,
            },
            None => Cover { most, covers: 1 },
        })
    }
}

impl CompiledHead {
    /// Gives the head's commits the run each starts: the head itself, then
    /// the run that starts the body.
    pub(crate) fn cover(&mut self, body: &CompiledBody) {
        let next = body
            .steps
            .first()
            .and_then(CompiledStep::commit)
            .and_then(|commit| commit.run);
        match self {
            Self::While { commit, .. } => commit.run = commit.cover(next, false),
            Self::For { guard, commit, .. } => {
                commit.run = commit.cover(next, false);
                if let Some((_, commit)) = guard {
                    commit.run = commit.cover(next, false);
                }
            }
        }
    }
}

impl CompiledBody {
    /// Gives each of the block's statements the run it starts, its arms'
    /// statements theirs: an allocating plan's statement admission.
    pub(crate) fn cover(&mut self) {
        let mut next = None;
        for step in self.steps.iter_mut().rev() {
            next = match step {
                CompiledStep::Write { commit, .. }
                | CompiledStep::Bind { commit, .. }
                | CompiledStep::Session { commit, .. }
                | CompiledStep::Store { commit, .. }
                | CompiledStep::Discard { commit, .. } => {
                    commit.run = commit.cover(next, false);
                    commit.run
                }
                CompiledStep::Continue(commit) => {
                    commit.run = commit.cover(next, true);
                    commit.run
                }
                CompiledStep::If {
                    commit,
                    then,
                    otherwise,
                    ..
                } => {
                    then.cover();
                    otherwise.cover();
                    commit.run = commit.cover(next, true);
                    commit.run
                }
                CompiledStep::Typed(_) | CompiledStep::Leave => None,
            };
        }
    }
}
