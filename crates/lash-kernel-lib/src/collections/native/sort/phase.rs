//! Borrow the semantic continuation across a callback, never an arena view.
use super::{Comparison, Merge, MergePosition};
use lash_kernel_doc::{Args, NativeError, Value};

/// A pending comparison with its actual guest buffers and ordered operands.
/// Dropping it leaves the merge untouched. This is native phase evidence;
/// source roles, callable purity, charges, peaks and invocation identity still
/// belong to the VM's source certificate. It cannot authorize publication.
pub struct ComparisonPhase<'sort> {
    merge: &'sort mut Merge,
    buffers: [Value; 2],
    callback: Value,
    operands: [Value; 2],
}

impl Merge {
    /// Bind only an already-pending comparison. The snapshot is the retained
    /// input sequence. Both guest buffers must contain exactly the initialized
    /// members described by the canonical schedule, including first-pass length.
    /// No `Args`, sequence view or scratch lease escapes this method.
    pub fn bind_comparison<'sort>(
        &'sort mut self,
        args: Args<'_>,
        snapshot: &[Value],
        buffers: [Value; 2],
        callback: Value,
        position: MergePosition,
        operands: [Value; 2],
    ) -> Result<Option<super::ComparisonPhase<'sort>>, NativeError> {
        if !matches!(callback, Value::Function(_) | Value::Closure(_))
            || !self.matches_source(args, snapshot, buffers, position, operands)?
        {
            return Ok(None);
        }
        Ok(Some(ComparisonPhase {
            merge: self,
            buffers,
            callback,
            operands,
        }))
    }

    fn matches_source(
        &self,
        args: Args<'_>,
        snapshot: &[Value],
        buffers: [Value; 2],
        position: MergePosition,
        operands: [Value; 2],
    ) -> Result<bool, NativeError> {
        let Some(Comparison { before, after }) = self.pending else {
            return Ok(false);
        };
        if self.position() != position
            || snapshot.len() != self.sorted.len()
            || !snapshot
                .get(before)
                .is_some_and(|value| value.same(&operands[0]))
            || !snapshot
                .get(after)
                .is_some_and(|value| value.same(&operands[1]))
        {
            return Ok(false);
        }
        let [Value::List(sorted), Value::List(out)] = buffers else {
            return Ok(false);
        };
        if sorted == out {
            return Ok(false);
        }
        for (buffer, positions) in buffers
            .iter()
            .zip([self.source_positions().0, self.source_positions().1])
        {
            let Some(values) = args.sequence(buffer)? else {
                return Ok(false);
            };
            if values.len() != positions.len() as u64 {
                return Ok(false);
            }
            for (at, index) in positions.iter().enumerate() {
                if !values.get(at as u64)?.is_some_and(|actual| {
                    snapshot
                        .get(*index)
                        .is_some_and(|expected| actual.same(expected))
                }) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

impl ComparisonPhase<'_> {
    pub fn position(&self) -> MergePosition {
        self.merge.position()
    }

    pub fn comparison(&self) -> Comparison {
        match self.merge.pending {
            Some(comparison) => comparison,
            None => unreachable!("a bound comparison remains pending"),
        }
    }

    pub fn operands(&self) -> [Value; 2] {
        self.operands
    }

    /// Reacquire the current views after the callback and advance exactly once.
    /// `take_right` is the canonical comparator decision, supplied only after
    /// the VM authenticates the actual completed callback and wins admission.
    /// A miss, cold read or abandoned candidate advances nothing.
    pub fn complete(
        self,
        args: Args<'_>,
        snapshot: &[Value],
        buffers: [Value; 2],
        callback: Value,
        position: MergePosition,
        take_right: bool,
    ) -> Result<bool, NativeError> {
        if !self.callback.same(&callback)
            || !self
                .buffers
                .iter()
                .zip(buffers)
                .all(|(expected, actual)| expected.same(&actual))
            || !self
                .merge
                .matches_source(args, snapshot, buffers, position, self.operands)?
        {
            return Ok(false);
        }
        self.merge.place(take_right)?;
        Ok(true)
    }
}
