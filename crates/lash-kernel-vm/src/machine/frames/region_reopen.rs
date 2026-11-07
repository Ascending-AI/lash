//! Recreate ephemeral source bindings from the admitted canonical continuation.
use super::*;
use crate::compile::room::{
    Phase, Point,
    publication::{Occurrence, Position},
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl Frame {
    /// Called after the ordinary frame restorer has admitted the canonical row.
    /// Suspended actions retain their existing resumption protocol. No body is
    /// read and no source event, occurrence or charge is executed here.
    pub(in crate::machine) fn reconstruct_region_frontier(
        &mut self,
        exe: &Executable,
        heap: &lash_kernel_state::kernel_internals::PagedHeap,
        logical: &std::rc::Rc<
            std::cell::RefCell<lash_kernel_state::kernel_internals::logical::Ledger>,
        >,
        task: lash_kernel_doc::TaskId,
    ) -> Result<(), HeapFault> {
        if self.source_frontier.is_some()
            || self.pending.peek().is_some()
            || self.awaiting.is_some()
            || self.cleaning.is_some()
        {
            return Ok(());
        }
        let code = exe.code(self.code);
        let source = &code.source_room;
        let roots = self
            .logical
            .as_ref()
            .ok_or_else(|| malformed("restored region has no invocation"))?;
        let at = if let Some(source) = &self.pending.source {
            source.at.clone()
        } else {
            match self.control.last() {
                Some(Control::Block { block, next }) => {
                    let block = exe.block(*block);
                    if *next == block.stmts.len() {
                        Point {
                            site: block.site.clone(),
                            phase: Phase::Leave,
                        }
                    } else {
                        let stmt = *block
                            .stmts
                            .get(*next)
                            .ok_or_else(|| malformed("restored region block index is invalid"))?;
                        source
                            .first_event(&exe.stmt(stmt).site)
                            .map_err(|_| malformed("restored statement has no source prefix"))?
                    }
                }
                Some(Control::While { stmt, .. }) => source
                    .first_event(&exe.stmt(*stmt).site.child(0))
                    .map_err(|_| malformed("restored while has no source test"))?,
                Some(Control::For { stmt, .. }) => Point {
                    site: exe.stmt(*stmt).site.clone(),
                    phase: Phase::Test,
                },
                Some(Control::Try { .. }) => return Ok(()),
                None => {
                    let statement = self.statement.resolve(heap.pages())?;
                    source
                        .first_event(&statement)
                        .map_err(|_| malformed("restored entry has no source prefix"))?
                }
            }
        };
        let document = exe
            .instances
            .get(&self.link)
            .ok_or_else(|| malformed("restored region Link is not admitted"))?;
        let kernel = lash_kernel_doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("restored region kernel is unsupported"))?;
        let loops = crate::machine::source_publication::source_loops(self, exe)?
            .ok_or_else(|| malformed("restored region loop coordinates are unavailable"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.canonical-region");
        at.site.encode(&mut encoding);
        encoding.unsigned(at.phase as u64);
        encoding.unsigned(roots.invocation().id());
        let position = Position {
            at: at.clone(),
            unpaid: Some(
                self.pending
                    .source
                    .as_ref()
                    .map_or(at, |source| source.unpaid.clone()),
            ),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task,
                invocation: roots.invocation().id(),
                loops,
                action: None,
            },
        };
        self.source_frontier = Some(
            source
                .frontier(
                    &logical.borrow(),
                    heap,
                    roots.invocation(),
                    self.link,
                    kernel,
                    position,
                )
                .map_err(|_| malformed("restored region source binding failed"))?,
        );
        Ok(())
    }
}

impl Frames {
    pub(in crate::machine) fn reconstruct_region_context(&mut self) -> Result<(), HeapFault> {
        let Some((task, logical)) = &self.logical else {
            if self.cache.is_empty() {
                return Ok(());
            }
            return Err(malformed("resident reopened frames have no source context"));
        };
        for (_, frame) in &mut self.cache {
            frame.logical = Some(crate::machine::logical_held::FrameRoots::restore(
                logical.clone(),
                *task,
                frame,
                &self.exe,
                &self.access,
            )?);
            frame.source_frontier = None;
        }
        Ok(())
    }
}
