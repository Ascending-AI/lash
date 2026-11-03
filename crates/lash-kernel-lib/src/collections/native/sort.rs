//! Key sorting uses the native ABI's single canonical merge continuation.
pub use lash_kernel_doc::arena::native::sort::{Comparison, ComparisonPhase, Merge, MergePosition};

use lash_kernel_doc::{NativeError, Value};
use std::cmp::Ordering;

/// All key sorts use this implementation, including input-only graph queries.
/// Scalar total-order cases retain the existing stable cached-key algorithms;
/// structural keys follow the same merge comparison schedule as list.sort.
pub(super) fn keys(
    ranks: &[Value],
    descending: bool,
    utf16: bool,
    mut compare: impl FnMut(usize, usize) -> Result<Option<Ordering>, NativeError>,
) -> Result<Vec<usize>, NativeError> {
    let integers = !utf16 && ranks.iter().all(|rank| matches!(rank, Value::Int(_)));
    let texts = !utf16 && !integers && ranks.iter().all(|rank| matches!(rank, Value::Text(_)));
    if integers || texts {
        let mut sorted: Vec<_> = (0..ranks.len()).collect();
        #[cfg(test)]
        let _permutation = crate::native_contract_laws::scratch_indices(&sorted);
        if integers {
            let key = |at: &usize| match ranks[*at] {
                Value::Int(value) => {
                    #[cfg(test)]
                    {
                        crate::native_contract_laws::CachedKey::new(value, *at)
                    }
                    #[cfg(not(test))]
                    {
                        value
                    }
                }
                _ => unreachable!("the complete key list was checked"),
            };
            if descending {
                sorted.sort_by_cached_key(|at| std::cmp::Reverse(key(at)));
            } else {
                sorted.sort_by_cached_key(key);
            }
            return Ok(sorted);
        } else {
            sorted.sort_by(|first, second| {
                let (Value::Text(first), Value::Text(second)) = (ranks[*first], ranks[*second])
                else {
                    unreachable!("the complete key list was checked")
                };
                let order = first.as_bytes().cmp(second.as_bytes());
                if descending { order.reverse() } else { order }
            });
            return Ok(sorted);
        }
    }
    let mut merge = Merge::new(ranks.len());
    #[cfg(test)]
    let _permutation = crate::native_contract_laws::scratch_indices(merge.positions().0);
    #[cfg(test)]
    let mut _output = crate::native_contract_laws::scratch_indices(merge.positions().1);
    while let Some(Comparison { before, after }) = merge.comparison() {
        let (first, second) = if descending {
            (before, after)
        } else {
            (after, before)
        };
        merge.place(compare(first, second)? == Some(Ordering::Less))?;
        #[cfg(test)]
        _output.resize(
            merge
                .positions()
                .1
                .iter()
                .map(|n| 16 + u64::from(usize::BITS - n.leading_zeros()).div_ceil(8))
                .sum(),
        );
    }
    merge.finish()
}
