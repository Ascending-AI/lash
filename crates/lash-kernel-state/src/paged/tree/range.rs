//! Range reads visit each intersecting tree node once.
use super::*;

impl<T: Cell> Tree<T> {
    /// Visits the requested rank range, loading each intersecting node once.
    pub(in crate::paged) fn visit_ranks(
        &self,
        access: &Access<'_>,
        start: u64,
        end: u64,
        visitor: &mut dyn FnMut(&T),
    ) -> Result<(), HeapFault> {
        fn walk<T: Cell>(
            branch: &Branch<T>,
            access: &Access<'_>,
            sequence: bool,
            start: u64,
            end: u64,
            visitor: &mut dyn FnMut(&T),
        ) -> Result<(), HeapFault> {
            let end = end.min(branch.summary.len);
            if start >= end {
                return Ok(());
            }
            match Tree::load_node(branch, access, sequence)?.node().view() {
                NodeView::Staged => unreachable!("loaded staged node"),
                NodeView::Leaf(rows) => {
                    for (_, value) in &rows[start as usize..end as usize] {
                        visitor(value);
                    }
                }
                NodeView::Index(rows) => {
                    let mut offset = 0u64;
                    for row in rows {
                        let next = offset
                            .checked_add(row.summary.len)
                            .ok_or_else(|| HeapFault::Malformed("tree rank overflow".into()))?;
                        if end <= offset {
                            break;
                        }
                        if start < next {
                            walk(
                                row,
                                access,
                                sequence,
                                start.saturating_sub(offset),
                                end - offset,
                                visitor,
                            )?;
                        }
                        offset = next;
                    }
                }
            }
            Ok(())
        }
        if let Some(root) = &self.root {
            walk(root, access, self.sequence, start, end, visitor)?;
        }
        Ok(())
    }
}
