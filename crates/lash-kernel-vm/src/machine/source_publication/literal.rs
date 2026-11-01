//! Deferred constants stage their complete backing before source acceptance.
use super::literal_storage::{Literal, PreparedLiteral};
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::machine::{Halt, KernelMachine, fault, registers::PreparedOutput, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::JournalEvent;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

struct LiteralResult<'bank, 'literal, 'heap> {
    bank: PreparedOutput<'bank>,
    recipe: Literal<'literal, 'heap>,
}
struct PreparedBankLiteral<'bank> {
    bank: PreparedOutput<'bank>,
    data: PreparedLiteral,
}
impl<'bank> StoragePreparation for LiteralResult<'bank, '_, '_> {
    type Prepared = PreparedBankLiteral<'bank>;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let Some(PreparedStorage {
            registration,
            storage,
            actual,
        }) = self.recipe.prepare(arena, actual)?
        else {
            return Ok(None);
        };
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedBankLiteral {
                bank: self.bank,
                data: storage,
            },
            actual,
        }))
    }
    fn facts(prepared: &Self::Prepared, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        <Literal<'_, '_> as StoragePreparation>::facts(&prepared.data, facts)
    }
}

impl KernelMachine {
    pub(in crate::machine) fn publish_source_literal(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let op = running.stream.ops[pc as usize];
        let Op::Const { c, .. } = op else {
            return Ok(false);
        };
        let crate::compile::Constant::Deferred(literal) = &running.stream.consts[c.ix()] else {
            return Ok(false);
        };
        let physical_room = self.capacity_headroom();
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, link, loops) = {
            let frame = self.frame(running.task)?;
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(false);
            };
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                loops,
            )
        };
        let Ok((at, unpaid, interval)) = source.compile_leaf(site) else {
            return Ok(false);
        };
        self.logical_flush()?;
        let Some(bank) = self.regs.prepare_source_output(running.stream, pc)? else {
            return Ok(false);
        };
        let output = bank.owner();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-literal");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            + usize::from(running.task != doc::TaskId::MAIN || !self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("precise source has no entered invocation"))?;
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops,
                action: None,
                departure: None,
            },
        };
        let next = Position {
            at: at.clone(),
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
        let published = Context {
            admission: Admission::Exact(MemoryLimit::of(
                self.bounds.memory_meter,
                &mut self.bounds.memory,
                &mut self.limiters,
            )),
            source: &source,
            link,
            kernel,
            logical: &self.logical,
            heap: &self.heap,
            arena: &mut arena,
            session: &mut self.session,
            resources: SourceResources {
                charged: &mut self.charged,
                counters: &mut task.counters,
                bound: if self.charging {
                    self.bounds.charge_limit
                } else {
                    u64::MAX
                },
                slice: self.slice_end,
                depth,
                limit: u64::from(self.bounds.call_depth),
                safe_point,
            },
            logical_memory: &self.logical_memory,
        }
        .publish(
            roots.invocation(),
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution([(at.clone(), LeafFacts::default())].into()),
                start,
                next,
                allowance: Allowance {
                    occurrences: vec![],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: false,
                    depth: 0,
                    safe_point,
                },
            },
            || Program::new(vec![JournalEvent::default()]),
            LiteralResult {
                bank,
                recipe: Literal {
                    literal,
                    link,
                    output,
                    at,
                    physical_room,
                    heap: &self.heap,
                },
            },
            |arena, _, edits, prepared| {
                debug_assert!(edits.is_empty());
                prepared.data.batch.publish(arena);
                prepared
                    .bank
                    .publish(prepared.data.value, prepared.data.holding);
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_literal_published(literal);
        }
        Ok(published)
    }
}
