//! Deferred constants stage their complete backing before source acceptance.
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::compile::room::{ExitKind, Measure, Owner as Selector, Phase, Point};
use crate::machine::{Halt, KernelMachine, fault, registers::PreparedOutput, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::{
    arena::PreparedBatch,
    kernel_internals::{
        logical::{Holding, JournalEvent, PublicationValue},
        stored_pages,
    },
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

struct LiteralResult<'bank, 'literal, 'heap> {
    bank: PreparedOutput<'bank>,
    literal: &'literal doc::Literal,
    link: lash_kernel_value::content_ids::LinkId,
    at: Point,
    physical_room: u64,
    heap: &'heap PagedHeap,
}
struct PreparedLiteral<'bank> {
    bank: PreparedOutput<'bank>,
    batch: PreparedBatch,
    value: doc::Value,
    holding: Holding,
    at: Point,
}
impl<'bank> StoragePreparation for LiteralResult<'bank, '_, '_> {
    type Prepared = PreparedLiteral<'bank>;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.inputs.is_empty()
            || !event.outputs.is_empty()
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
            || !event.scratch.is_empty()
        {
            return Ok(None);
        }
        use doc::{Literal, Value, heap::PageValue};
        // Page construction may fail. Do it before reserving any nursery slot.
        let page = match self.literal {
            Literal::Null => PageValue::Null,
            Literal::Absent => PageValue::Absent,
            Literal::Bool(value) => PageValue::Bool(*value),
            Literal::Float(value) => PageValue::Float(*value),
            Literal::Int(value) => {
                PageValue::Int(stored_pages::int(self.heap.pages(), value.clone())?)
            }
            Literal::Text(value) => PageValue::Text(stored_pages::text(self.heap.pages(), value)?),
            Literal::Bytes(value) => {
                PageValue::Bytes(stored_pages::bytes(self.heap.pages(), value.as_slice())?)
            }
            Literal::Function(function) => {
                PageValue::Function(Box::new(lash_kernel_value::names::CallableId {
                    link: self.link,
                    function: *function,
                }))
            }
        };
        let mut nursery = doc::Nursery::unaccounted(arena.objects());
        let value = match self.literal {
            Literal::Null => Value::Null,
            Literal::Absent => Value::Absent,
            Literal::Bool(value) => Value::Bool(*value),
            Literal::Float(value) => Value::Float(*value),
            Literal::Int(value) => nursery.integer(value.clone()),
            Literal::Text(value) => nursery.text(value),
            Literal::Bytes(value) => nursery.bytes(value.as_slice()),
            Literal::Function(function) => nursery.function(lash_kernel_value::names::CallableId {
                link: self.link,
                function: *function,
            }),
        };
        // Compatibility eligibility only until the complete logical cutover.
        // This physical estimate supplies no guest price or refusal.
        if arena.install_bound(nursery.built()) > self.physical_room {
            nursery.discard();
            return Ok(None);
        }
        let installation = arena
            .prepare_install(nursery.into_new())
            .map_err(native_fault)?;
        let (registration, batch) = installation.into_parts();
        let holding = Holding::from_page(self.heap.pages(), &page)?;
        let mut event = event.clone();
        event
            .outputs
            .push((self.bank.owner(), PublicationValue::Page(page)));
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedLiteral {
                bank: self.bank,
                batch,
                value,
                holding,
                at: self.at,
            },
            actual: Program::new(vec![event]),
        }))
    }
    fn facts(prepared: &Self::Prepared, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&prepared.at)
            .ok_or_else(|| malformed("literal facts disappeared"))?;
        facts.values.insert(
            Measure::Local(Selector::Operand(prepared.at.site.clone())),
            i128::from(prepared.holding.bytes()),
        );
        Ok(())
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
        let Ok(unpaid) = source.next_after_commit(site) else {
            return Ok(false);
        };
        let at = Point {
            site: (**site).clone(),
            phase: Phase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
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
            admission: Admission::Exact,
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
            memory_bound: self.bounds.memory,
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
                literal,
                link,
                at,
                physical_room,
                heap: &self.heap,
            },
            |arena, _, edits, prepared| {
                debug_assert!(edits.is_empty());
                prepared.batch.publish(arena);
                prepared.bank.publish(prepared.value, prepared.holding);
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_literal_published(literal);
        }
        Ok(published)
    }
}
