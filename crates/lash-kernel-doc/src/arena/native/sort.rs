//! The canonical stable merge continuation shared by natives and the VM.
//! Positions are semantic input positions. Arena views end before callback
//! entry; source roles and resource acceptance remain the VM's authority.
mod phase;
pub use phase::ComparisonPhase;

use crate::NativeError;

/// Original input positions, in the canonical left/right callback order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comparison {
    pub before: usize,
    pub after: usize,
}

/// Source loop locals at the pending comparison. Core binds these roles to
/// the source recovery map, independently of physical register placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergePosition {
    pub width: usize,
    pub start: usize,
    pub left: usize,
    pub right: usize,
    pub dest: usize,
    /// The guest body's output list grows during the first pass. Unwritten
    /// native buffer positions do not become guest members or holes.
    pub output_len: usize,
}

/// A merge continuation contains semantic positions only. Its buffers must be
/// retained as native continuation owners across a callback or cold handoff.
/// No arena handle or view is cached here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    sorted: Vec<usize>,
    out: Vec<usize>,
    width: usize,
    start: usize,
    left: usize,
    right: usize,
    dest: usize,
    pending: Option<Comparison>,
}

impl Merge {
    pub fn new(len: usize) -> Self {
        Self::from_order((0..len).collect())
    }

    fn from_order(sorted: Vec<usize>) -> Self {
        let len = sorted.len();
        Self {
            sorted,
            out: vec![0; len],
            width: 1,
            start: 0,
            left: 0,
            right: 1.min(len),
            dest: 0,
            pending: None,
        }
    }

    /// Repeated reads while a callback is suspended return the same operands.
    /// Copies and swaps occur only between comparisons.
    pub fn comparison(&mut self) -> Option<Comparison> {
        if let Some(pending) = self.pending {
            return Some(pending);
        }
        let n = self.sorted.len();
        while self.width < n {
            if self.start >= n {
                std::mem::swap(&mut self.sorted, &mut self.out);
                self.width = self.width.saturating_mul(2);
                self.start = 0;
                self.left = 0;
                self.right = self.width.min(n);
                self.dest = 0;
                continue;
            }
            let mid = self.start.saturating_add(self.width).min(n);
            let end = mid.saturating_add(self.width).min(n);
            if self.left < mid && self.right < end {
                let position = self.position();
                let (sorted, _) = self.source_positions();
                let comparison = Comparison {
                    before: sorted[position.left],
                    after: sorted[position.right],
                };
                self.pending = Some(comparison);
                return Some(comparison);
            }
            self.out[self.dest..self.dest + mid - self.left]
                .copy_from_slice(&self.sorted[self.left..mid]);
            self.dest += mid - self.left;
            self.out[self.dest..self.dest + end - self.right]
                .copy_from_slice(&self.sorted[self.right..end]);
            self.start = end;
            self.left = end;
            self.right = end.saturating_add(self.width).min(n);
            self.dest = end;
        }
        None
    }

    /// Ties and unordered numeric comparisons take the left run. The caller
    /// converts its canonical comparator answer to `take_right` exactly once.
    pub fn place(&mut self, take_right: bool) -> Result<(), NativeError> {
        let comparison = self.pending.take().ok_or(NativeError::Malformed(
            "sort answer without a pending comparison",
        ))?;
        self.out[self.dest] = if take_right {
            self.right += 1;
            comparison.after
        } else {
            self.left += 1;
            comparison.before
        };
        self.dest += 1;
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<usize>, NativeError> {
        if self.width < self.sorted.len() || self.pending.is_some() {
            return Err(NativeError::Malformed("unfinished sort continuation"));
        }
        Ok(self.sorted)
    }

    pub fn position(&self) -> MergePosition {
        MergePosition {
            width: self.width,
            start: self.start,
            left: self.left,
            right: self.right,
            dest: self.dest,
            output_len: if self.width == 1 {
                self.dest
            } else {
                self.out.len()
            },
        }
    }

    /// Actual member positions of the two guest buffers at this boundary.
    /// The native's preallocated tail is scratch, not initialized guest data.
    pub fn source_positions(&self) -> (&[usize], &[usize]) {
        let (sorted, out) = self.positions();
        (sorted, &out[..self.position().output_len])
    }

    /// Both index buffers are logical continuation holdings. During a guest
    /// callback these are not an arena scratch lease.
    pub fn positions(&self) -> (&[usize], &[usize]) {
        (&self.sorted, &self.out)
    }
}
