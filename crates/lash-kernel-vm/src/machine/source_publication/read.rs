//! Member reads publish their checked operands and canonical result together.
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::compile::room::{ExitKind, Measure, Owner as Selector, Phase, Point};
use crate::machine::stream::Running;
use crate::machine::{Halt, KernelMachine, fault, registers::PreparedOutput};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{
    Holding, JournalEvent, Ledger, PublicationValue, RootValue,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

struct ReadResult<'bank, 'heap> {
    bank: PreparedOutput<'bank>,
    value: doc::Value,
    at: Point,
    release: Box<[Selector]>,
    ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    heap: &'heap PagedHeap,
}
struct PreparedRead<'bank> {
    bank: PreparedOutput<'bank>,
    holding: Holding,
    at: Point,
    release: Box<[Selector]>,
    released: u64,
}
impl<'bank> StoragePreparation for ReadResult<'bank, '_> {
    type Prepared = PreparedRead<'bank>;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.outputs.is_empty()
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
            || !event.scratch.is_empty()
        {
            return Ok(None);
        }
        let mut registration = arena.registration().map_err(native_fault)?;
        let holding = Holding::from_native(registration.arena(), self.heap.pages(), &self.value)?;
        let released = self.ledger.borrow().release_cost_preserving(
            registration.arena(),
            self.heap,
            &event.inputs,
            &[(self.bank.owner(), RootValue::Value(&self.value))],
        )?;
        let mut event = event.clone();
        event
            .outputs
            .push((self.bank.owner(), PublicationValue::Native(self.value)));
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedRead {
                bank: self.bank,
                holding,
                at: self.at,
                release: self.release,
                released,
            },
            actual: Program::new(vec![event]),
        }))
    }
    fn facts(prepared: &Self::Prepared, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&prepared.at)
            .ok_or_else(|| malformed("read facts disappeared"))?;
        facts.values.insert(
            Measure::Local(Selector::Operand(prepared.at.site.clone())),
            i128::from(prepared.holding.bytes()),
        );
        facts.values.insert(
            Measure::Release(prepared.release.clone()),
            i128::from(prepared.released),
        );
        Ok(())
    }
}

impl KernelMachine {
    pub(in crate::machine) fn publish_source_read(
        &mut self,
        running: &Running<'_>,
        pc: u32,
        value: doc::Value,
    ) -> Result<bool, Halt> {
        let op = running.stream.ops[pc as usize];
        if !matches!(op, Op::Field { .. } | Op::Index { .. }) {
            return Ok(false);
        }
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
        let inputs = bank.owners();
        let values = bank.values();
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("read event disappeared"))?
            .inputs
            .clone();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-read");
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
                facts: LeafExecution(
                    [(
                        at.clone(),
                        LeafFacts {
                            dependencies: values
                                .iter()
                                .chain(std::iter::once(&value))
                                .filter_map(doc::Value::handle)
                                .collect(),
                            ..Default::default()
                        },
                    )]
                    .into(),
                ),
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
            || {
                Program::new(vec![JournalEvent {
                    inputs: inputs.clone(),
                    ..Default::default()
                }])
            },
            ReadResult {
                bank,
                value,
                at,
                release,
                ledger: self.logical.ledger(),
                heap: &self.heap,
            },
            |_, _, edits, prepared| {
                debug_assert!(edits.is_empty());
                prepared.bank.publish(value, prepared.holding);
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_read_published(matches!(
                op,
                Op::Index { .. }
            ));
        }
        Ok(published)
    }
}
