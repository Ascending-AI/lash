//! Pure source reads install their bank occurrence in the accepted journal.
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::compile::room::{ExitKind, Measure, Owner as Selector, Phase, Point};
use crate::machine::{Control, Halt, KernelMachine, SlotState, fault};
use crate::machine::{logical_held::Projection, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, PublicationValue};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_leaf(
        &mut self,
        running: &Running<'_>,
        pc: u32,
        value: doc::Value,
    ) -> Result<bool, Halt> {
        let op = running.stream.ops[pc as usize];
        if !matches!(op, Op::Const { .. } | Op::Local { .. } | Op::Session { .. })
            || matches!(op, Op::Const { .. }) && value.handle().is_some()
        {
            return Ok(false);
        }
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, link, cell) = {
            let frame = self.frame(running.task)?;
            // Loop headers and active departures need their own exact runtime
            // occurrence. This cohort has neither; it never invents one.
            if frame.cleaning.is_some()
                || frame.control.iter().any(|control| {
                    matches!(
                        control,
                        Control::For { .. } | Control::While { .. } | Control::Try { .. }
                    )
                })
            {
                return Ok(false);
            }
            let cell = match op {
                Op::Local { local, .. } => match frame.slot(local.at) {
                    SlotState::Cell(cell) => Some(*cell),
                    _ => None,
                },
                _ => None,
            };
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                cell,
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
        let Some(bank) = self.regs.prepare_source_leaf(
            running.stream,
            pc,
            value,
            &Projection {
                arena: &self.arena,
                heap: &self.heap,
            },
        )?
        else {
            return Ok(false);
        };
        let output = bank.owner();
        let bytes = bank.bytes();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-leaf");
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
                loops: vec![],
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
                            dependencies: value.handle().into_iter().chain(cell).collect(),
                            values: [(
                                Measure::Local(Selector::Operand(at.site)),
                                i128::from(bytes),
                            )]
                            .into(),
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
                    outputs: vec![(output, PublicationValue::Native(value))],
                    ..Default::default()
                }])
            },
            Reserve(move |_: &mut Arena| Ok(Some(bank))),
            |_, _, edits, bank| {
                debug_assert!(edits.is_empty());
                bank.publish();
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_source_published();
            if matches!(op, Op::Session { .. }) {
                crate::machine::islands::witness::precise_session_read_published();
            }
        }
        Ok(published)
    }
}
